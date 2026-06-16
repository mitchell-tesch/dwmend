//! Process-level concerns shared by every subcommand: tracing init,
//! the panic-to-tracing bridge, and the per-session single-instance
//! guard for the daemon.

use crate::config;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

/// Configure tracing: a daily-rolling file appender under
/// `%LOCALAPPDATA%\dwmend\dwmend.log.<date>` plus a colorised stderr
/// layer. The returned guard must be kept alive for the duration of the
/// process so the non-blocking writer can drain on shutdown.
pub fn setup_tracing() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = config::data_dir()?;
    std::fs::create_dir_all(&dir)?;
    let appender = tracing_appender::rolling::daily(&dir, "dwmend.log");
    let (nb, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        // File layer — non-blocking, may delay-flush briefly.
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(nb),
        )
        // Console layer — direct, unbuffered writes to stderr so the operator
        // sees every event in real time even if stdout is being piped.
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stderr),
        )
        .try_init()
        .map_err(|e| eyre!("tracing init: {e}"))?;
    Ok(guard)
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| match info.location() {
        Some(loc) => tracing::error!(
            file = loc.file(),
            line = loc.line(),
            column = loc.column(),
            "panic: {info}"
        ),
        None => tracing::error!("panic: {info}"),
    }));
}

/// RAII guard around the per-session named mutex used for the
/// single-instance check. Holding the mutex prevents another `dwmend.exe`
/// from starting; dropping the guard (process exit, daemon shutdown)
/// releases ownership so the next launch succeeds immediately.
///
/// `HANDLE` is `*mut c_void` which is not `Send`, but the daemon's main
/// thread is the only one that touches this guard, so we explicitly mark
/// it `Send` to satisfy any future thread-bound storage. The OS does not
/// care which thread calls `CloseHandle`.
pub struct InstanceGuard(HANDLE);

// SAFETY: HANDLE is a kernel object reference; CloseHandle is documented
// thread-safe with respect to the handle being closed.
unsafe impl Send for InstanceGuard {}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: handle was returned by CreateMutexW above and has not been
        // closed elsewhere. The OS releases the named mutex when the last
        // process handle closes (or on process exit).
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Single-instance check using a per-session named mutex.
///
/// Compared to the previous `sysinfo`-based process-table scan, this is:
/// * **Atomic** — no race between "scan" and "begin running"; the OS
///   either gives us ownership or returns `ERROR_ALREADY_EXISTS`.
/// * **Fast** — a single Win32 call instead of loading every process's
///   metadata (which can take 100-300 ms on a busy box).
/// * **Self-cleaning** — the OS releases the mutex on process exit even
///   if we crash, so no stale lock file or PID file to clean up.
///
/// The `Local\` namespace prefix scopes the mutex to the current login
/// session, which is what we want: another user's dwmend running on the
/// same machine shouldn't block ours. The `-v1` suffix lets us bump the
/// name if a future change ever needs to invalidate the old lock.
pub fn ensure_single_instance() -> Result<InstanceGuard> {
    // SAFETY: lpmutexattributes None (default security descriptor),
    // binitialowner true (we own it on success), name is a static UTF-16
    // literal so the pointer is valid for the call.
    let handle = unsafe { CreateMutexW(None, true, w!("Local\\DwmendDaemon-mutex-v1")) };
    // Capture last error BEFORE any other Win32 call (eyre/tracing may run
    // code that resets it). `CreateMutexW` succeeds even when the named
    // object already exists — the distinguishing signal is GetLastError().
    let last_err = unsafe { GetLastError() };
    let handle = handle.map_err(|e| eyre!("CreateMutexW failed: {e}"))?;
    if last_err == ERROR_ALREADY_EXISTS {
        // SAFETY: handle came from CreateMutexW above; closing it does NOT
        // release the underlying named mutex (the other process still owns
        // it) — it just decrements our local reference count.
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(eyre!(
            "another dwmend.exe is already running — exit it before launching a new one"
        ));
    }
    Ok(InstanceGuard(handle))
}
