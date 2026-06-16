//! `AllowSetForegroundWindow(GetCurrentProcessId())`.
//!
//! Without this, the OS aggressively rejects `SetForegroundWindow` calls from
//! processes that don't currently hold focus — which is exactly our situation
//! every time we're moving focus to a window the user has not just clicked.
//!
//! ## Failure mode
//!
//! The call itself requires the caller to currently hold (or have just lost)
//! the foreground privilege. If DWMend launches from a terminal that has lost
//! focus before this call executes, every attempt fails with `E_ACCESSDENIED`.
//! That is **non-fatal**: `SetForegroundWindow` itself may still work in
//! practice, and any individual failure is already logged at the call site.
//! So we log a warning and continue rather than aborting the daemon.

use crate::Result;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

/// Try up to `retries` times to allow ourselves to set the foreground window.
/// Always returns `Ok(())` — failures only emit a warning.
pub fn allow_set_foreground(retries: u32) -> Result<()> {
    // SAFETY: GetCurrentProcessId is always safe.
    let pid = unsafe { GetCurrentProcessId() };
    let mut last_err = None;
    for attempt in 0..retries {
        // SAFETY: AllowSetForegroundWindow with a valid PID is safe.
        match unsafe { AllowSetForegroundWindow(pid) } {
            Ok(()) => {
                if attempt > 0 {
                    tracing::info!(attempt, "AllowSetForegroundWindow succeeded after retry");
                }
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    tracing::warn!(
        attempts = retries,
        error = ?last_err,
        "AllowSetForegroundWindow failed; SetForegroundWindow may be flaky \
         (this is normal when DWMend launches without focus)"
    );
    Ok(())
}
