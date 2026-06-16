//! Process-wide DPI awareness.
//!
//! Must be called once at the very start of `main`, before *any* HWND is
//! created or queried. Per-Monitor v2 means the OS will NOT virtualise
//! coordinates for us — every monitor reports its own DPI and we are
//! responsible for using it. That's exactly what we want for a tiler that
//! spans mixed-DPI displays.
//!
//! ## Idempotency
//!
//! Our embedded manifest already sets PerMonitorV2 at process load time,
//! so this runtime call is technically redundant — but it's a useful guard
//! against builds that lose the manifest (e.g. a test harness, a future
//! `--no-manifest` flag, or any tool that re-links our object files).
//!
//! Windows refuses to **change** process DPI awareness once set, returning
//! `E_ACCESSDENIED`. We treat that as success: it means the manifest beat
//! us to the punch with the right value.

use crate::Result;
use color_eyre::eyre::eyre;
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
};

/// Enable PerMonitorV2 awareness for this process if not already set.
///
/// Returns `Ok(())` even if the awareness was already set (by manifest or a
/// prior call). Returns `Err` only on truly unexpected failures.
pub fn set_per_monitor_v2() -> Result<()> {
    // SAFETY: GetThreadDpiAwarenessContext is a thread-safe Win32 call with
    // no parameters. The returned handle is a sentinel pointer, not heap.
    let current = unsafe { GetThreadDpiAwarenessContext() };
    // AreDpiAwarenessContextsEqual normalises the sentinel forms — never use
    // raw == here, the OS may hand back a value-equivalent but bit-different
    // pointer (especially when the awareness was set via manifest).
    // SAFETY: both arguments are valid Win32 DPI awareness contexts.
    let already_v2 = unsafe {
        AreDpiAwarenessContextsEqual(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).as_bool()
    };
    if already_v2 {
        tracing::info!("DPI awareness already PerMonitorV2 (manifest); skipping");
        return Ok(());
    }

    // SAFETY: SetProcessDpiAwarenessContext is a thread-safe Win32 call.
    match unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == windows::Win32::Foundation::E_ACCESSDENIED => {
            tracing::warn!(
                "DPI awareness was preset to a non-PerMonitorV2 value; \
                 layout math may be off on mixed-DPI setups"
            );
            Ok(())
        }
        Err(e) => Err(eyre!("SetProcessDpiAwarenessContext failed: {e}")),
    }
}
