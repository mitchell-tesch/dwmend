//! Named-pipe IPC server for the daemon.
//!
//! ## Why
//!
//! Hotkeys are great for keyboard-driven workflows but they cannot be
//! scripted from PowerShell, AHK, StreamDeck, etc. A simple line-oriented
//! pipe at `\\.\pipe\DwmendDaemon-v1` lets external tooling drive the WM
//! without the hassle of a HTTP server, COM registration, or D-Bus.
//!
//! ## Protocol
//!
//! One newline-terminated request line per connection; one newline-terminated
//! response line; then the server closes the pipe and accepts the next
//! client. Both sides are UTF-8 JSON. See `dwmend::ipc` for the schema.
//!
//! ## Threading
//!
//! A single dedicated server thread:
//! 1. Calls [`CreateNamedPipeW`] (max 1 concurrent instance — we serialise).
//! 2. Blocks on [`ConnectNamedPipe`] until a client opens the pipe.
//! 3. Reads one line, forwards it onto the `Receiver<IpcRequest>` along
//!    with a single-shot reply channel.
//! 4. Waits up to 5 s for a reply from the daemon's handler thread.
//! 5. Writes the reply + flushes + disconnects.
//! 6. Loops back to step 1.
//!
//! On process exit the thread is reaped by the OS along with the pipe
//! handle — no explicit shutdown.

use crate::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use std::sync::OnceLock;
use std::time::Duration;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE, FlushFileBuffers, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

/// The pipe name. The `v1` suffix lets us bump the protocol later without
/// breaking older CLIs that try to connect to a newer daemon (or vice versa).
pub const PIPE_NAME: &str = r"\\.\pipe\DwmendDaemon-v1";

/// One request line + a one-shot reply channel. The host (daemon) drains
/// the receiver, builds a response, and sends it back through `reply`.
pub struct IpcRequest {
    pub line: String,
    pub reply: Sender<String>,
}

static EVENT_TX: OnceLock<Sender<IpcRequest>> = OnceLock::new();

// ---- public API -------------------------------------------------------------

/// Spawn the IPC server thread. Safe to call at most once per process.
pub fn start() -> Result<Receiver<IpcRequest>> {
    let (tx, rx) = unbounded();
    if EVENT_TX.set(tx).is_err() {
        return Err(eyre!("ipc::start called more than once"));
    }
    std::thread::Builder::new()
        .name("dwmend-ipc".into())
        .spawn(run_server_thread)
        .map_err(|e| eyre!("failed to spawn dwmend-ipc thread: {e}"))?;
    Ok(rx)
}

/// Client-side helper: open the pipe, write one line, read one line, close.
/// Used by the `dwmend cmd` / `dwmend query` subcommands.
///
/// Retries briefly with `WaitNamedPipeW` if the server is busy serving a
/// previous client, so back-to-back script calls never spuriously fail.
pub fn client_send(request: &str) -> Result<String> {
    let name = wide(PIPE_NAME);
    let handle = open_client_pipe(&name)?;

    // Build the payload — exactly one line, '\n'-terminated.
    let mut payload = request.to_string();
    if !payload.ends_with('\n') {
        payload.push('\n');
    }

    let mut written = 0u32;
    // SAFETY: handle came from CreateFileW above; payload outlives the
    // call.
    unsafe { WriteFile(handle, Some(payload.as_bytes()), Some(&mut written), None) }
        .map_err(|e| eyre!("WriteFile: {e}"))?;
    // SAFETY: handle valid; flush is a no-arg op.
    let _ = unsafe { FlushFileBuffers(handle) };

    let response = read_line(handle);
    // SAFETY: handle came from CreateFileW.
    let _ = unsafe { CloseHandle(handle) };
    response
}

// ---- server -----------------------------------------------------------------

/// RAII wrapper around a `LocalAlloc`-owned security descriptor so the
/// memory is freed if the server thread ever unwinds. The descriptor is
/// referenced by every subsequent `CreateNamedPipeW` call \u2014 the kernel
/// copies it into each new pipe object, so the lifetime here only needs
/// to outlive the calls themselves.
struct SecurityDescriptor {
    psd: PSECURITY_DESCRIPTOR,
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.psd.0.is_null() {
            // SAFETY: psd was allocated by
            // ConvertStringSecurityDescriptorToSecurityDescriptorW, whose
            // documented buffer-ownership contract is "free with
            // LocalFree". HLOCAL is a transparent pointer type.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.psd.0)));
            }
            self.psd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        }
    }
}

/// Build a security descriptor whose DACL grants the current user
/// (and only the current user) full access to the pipe.
///
/// Without this the pipe inherits the token's default DACL, which
/// allows ANY same-user process to send commands or scrape window
/// state \u2014 the audit's flagged Tier-2 risk. SDDL form:
///
/// ```text
///   D:P(A;;GA;;;<user-sid>)
/// ```
///
/// `D:P` = DACL, protected from inheritance. `A` = Allow ACE. `GA` =
/// Generic All. The trailing SID is the current user's, looked up via
/// the process token. We deliberately do NOT also grant
/// `BUILTIN\\Administrators` \u2014 a personal WM has no admin-tooling
/// surface, and admins can elevate into the user session anyway.
fn owner_only_security_descriptor() -> Result<SecurityDescriptor> {
    let sid_string = current_user_sid_string()?;
    let sddl = format!("D:P(A;;GA;;;{sid_string})");
    let sddl_w = wide(&sddl);

    let mut psd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    // SAFETY: sddl_w is a null-terminated UTF-16 string; psd is a valid
    // out-pointer; the optional size out-param is left None because we
    // don't need to know the descriptor size \u2014 the kernel does.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
    }
    .map_err(|e| eyre!("ConvertStringSecurityDescriptorToSecurityDescriptorW({sddl}): {e}"))?;

    if psd.0.is_null() {
        return Err(eyre!("ConvertStringSecurityDescriptorToSecurityDescriptorW returned NULL"));
    }
    Ok(SecurityDescriptor { psd })
}

/// Look up the current process's user SID and convert it to its SDDL
/// string form (e.g. `S-1-5-21-...-1001`). Allocates internally; all
/// allocations are freed before return.
fn current_user_sid_string() -> Result<String> {
    // 1) Open the process token.
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess is an infallible pseudo-handle; token
    // is a valid out-param.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| eyre!("OpenProcessToken: {e}"))?;
    // RAII: ensure CloseHandle runs on every exit path including ?.
    struct TokenGuard(HANDLE);
    impl Drop for TokenGuard {
        fn drop(&mut self) {
            // SAFETY: handle was returned by OpenProcessToken.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
    let _token_guard = TokenGuard(token);

    // 2) Probe the size of the TokenUser info.
    let mut needed = 0u32;
    // SAFETY: token is valid; first call deliberately undersized so the
    // kernel reports the required buffer size in `needed` and returns
    // ERROR_INSUFFICIENT_BUFFER \u2014 we ignore the Result.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return Err(eyre!("GetTokenInformation reported zero size for TokenUser"));
    }
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: buf has `needed` bytes; pointer cast is to a documented
    // out-buffer for TOKEN_USER.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|e| eyre!("GetTokenInformation(TokenUser): {e}"))?;

    // 3) Extract the SID pointer from the returned TOKEN_USER struct.
    // SAFETY: kernel filled `buf` with a TOKEN_USER followed by the SID
    // bytes; the cast respects layout.
    let token_user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let sid = token_user.User.Sid;
    if sid.0.is_null() {
        return Err(eyre!("TOKEN_USER returned a NULL SID"));
    }

    // 4) Convert to its SDDL string form. The returned PWSTR is owned
    // by the caller and must be freed with LocalFree.
    let mut sid_str = PWSTR::null();
    // SAFETY: sid is non-null per check above; sid_str is a valid out-pointer.
    unsafe { ConvertSidToStringSidW(sid, &mut sid_str) }
        .map_err(|e| eyre!("ConvertSidToStringSidW: {e}"))?;
    if sid_str.is_null() {
        return Err(eyre!("ConvertSidToStringSidW returned NULL"));
    }
    // SAFETY: sid_str is null-terminated per the API contract.
    let owned = unsafe { sid_str.to_string() }
        .map_err(|e| eyre!("PWSTR::to_string: {e}"));
    // Free regardless of whether to_string succeeded.
    // SAFETY: pointer was allocated by ConvertSidToStringSidW.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid_str.0 as *mut _)));
    }
    owned
}

fn run_server_thread() {
    tracing::info!(pipe = PIPE_NAME, "ipc server started");

    // Build the owner-only security descriptor once. If this fails we
    // fall back to NULL (default DACL: any same-user process can
    // connect) and log a warning, so a Win32 quirk on some host can
    // never block the daemon from booting \u2014 it just degrades to
    // pre-Tier-2 behaviour.
    let sd = match owner_only_security_descriptor() {
        Ok(sd) => Some(sd),
        Err(e) => {
            tracing::warn!(error = %e,
                "ipc: failed to build owner-only ACL; pipe will use default DACL");
            None
        }
    };

    loop {
        let pipe = match create_pipe(sd.as_ref()) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error = %e, "ipc: failed to create pipe; sleeping 1 s");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if let Err(e) = handle_one_connection(pipe) {
            tracing::debug!(error = %e, "ipc: connection ended");
        }
        // SAFETY: handle was returned by CreateNamedPipeW above.
        let _ = unsafe { CloseHandle(pipe) };
    }
}

fn create_pipe(sd: Option<&SecurityDescriptor>) -> std::result::Result<HANDLE, String> {
    let name = wide(PIPE_NAME);
    // Build a SECURITY_ATTRIBUTES referencing the descriptor when we
    // have one. Kept on the stack: CreateNamedPipeW captures the
    // descriptor into the kernel object, so the SA struct only needs
    // to live for the duration of the call.
    let sa = sd.map(|sd| SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.psd.0,
        bInheritHandle: false.into(),
    });
    // SAFETY: name is a null-terminated UTF-16 string; other args are
    // documented integer constants. `sa`, when present, points to a
    // valid descriptor allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    let h = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1, // max concurrent instances \u2014 we serialise
            4096,
            4096,
            0,
            sa.as_ref().map(|x| x as *const _),
        )
    };
    if h.is_invalid() {
        // SAFETY: GetLastError has no preconditions.
        let err = unsafe { GetLastError() };
        return Err(format!("CreateNamedPipeW failed: WIN32_ERROR({})", err.0));
    }
    Ok(h)
}

fn handle_one_connection(pipe: HANDLE) -> std::result::Result<(), String> {
    // SAFETY: pipe is a valid server-side named pipe handle.
    let connect_result = unsafe { ConnectNamedPipe(pipe, None) };
    if let Err(e) = connect_result {
        // ERROR_PIPE_CONNECTED is benign — it means the client got in
        // between CreateNamedPipeW and ConnectNamedPipe. We can proceed
        // to read.
        // SAFETY: GetLastError has no preconditions.
        let last = unsafe { GetLastError() };
        if last != ERROR_PIPE_CONNECTED {
            return Err(format!("ConnectNamedPipe: {e}"));
        }
    }

    let line = match read_line(pipe) {
        Ok(s) => s,
        Err(e) => {
            // SAFETY: pipe is a valid server-side handle.
            let _ = unsafe { DisconnectNamedPipe(pipe) };
            return Err(format!("read_line: {e}"));
        }
    };

    let (reply_tx, reply_rx) = bounded::<String>(1);
    if let Some(tx) = EVENT_TX.get() {
        // try_send so a clogged channel cannot stall the server thread;
        // the client will just see the "timeout" response below.
        let _ = tx.try_send(IpcRequest {
            line,
            reply: reply_tx,
        });
    }

    let response = match reply_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => r#"{"ok":false,"error":"timeout"}"#.to_string(),
    };

    let mut payload = response;
    payload.push('\n');
    let mut written = 0u32;
    // SAFETY: pipe is a valid server-side handle; payload outlives the call.
    let _ = unsafe { WriteFile(pipe, Some(payload.as_bytes()), Some(&mut written), None) };
    let _ = unsafe { FlushFileBuffers(pipe) };
    let _ = unsafe { DisconnectNamedPipe(pipe) };
    Ok(())
}

// ---- client -----------------------------------------------------------------

fn open_client_pipe(name: &[u16]) -> Result<HANDLE> {
    // Try a few times: the server may be in between DisconnectNamedPipe
    // and CreateNamedPipeW for a fraction of a millisecond, and we don't
    // want that to spuriously fail.
    let mut last_err: Option<windows::core::Error> = None;
    for _ in 0..10 {
        // SAFETY: name is a null-terminated UTF-16 string.
        match unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        } {
            Ok(h) => return Ok(h),
            Err(e) => {
                last_err = Some(e);
                // Wait up to 200 ms for the next server instance.
                // SAFETY: name is null-terminated UTF-16.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 200) };
            }
        }
    }
    Err(eyre!(
        "CreateFileW({PIPE_NAME}): {} -- is the daemon running?",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into())
    ))
}

// ---- shared -----------------------------------------------------------------

fn read_line(h: HANDLE) -> Result<String> {
    let mut out = Vec::with_capacity(256);
    let mut buf = [0u8; 1];
    loop {
        let mut got = 0u32;
        // SAFETY: buf is a valid mutable byte slice; got is a valid
        // out-param.
        let r = unsafe { ReadFile(h, Some(&mut buf), Some(&mut got), None) };
        match r {
            Ok(()) => {
                if got == 0 {
                    break;
                }
                if buf[0] == b'\n' {
                    break;
                }
                if buf[0] == b'\r' {
                    continue;
                }
                out.push(buf[0]);
                if out.len() > 64 * 1024 {
                    return Err(eyre!("request too large (>64 KB)"));
                }
            }
            Err(e) => {
                // SAFETY: GetLastError has no preconditions.
                let last = unsafe { GetLastError() };
                if last == ERROR_BROKEN_PIPE {
                    break;
                }
                return Err(eyre!("ReadFile: {e}"));
            }
        }
    }
    String::from_utf8(out).map_err(|e| eyre!("non-UTF8 input: {e}"))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
