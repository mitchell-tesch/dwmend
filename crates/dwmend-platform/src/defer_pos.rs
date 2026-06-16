//! Atomic batch window positioning via `BeginDeferWindowPos`.
//!
//! This is **the** smoothness primitive. Moving N windows individually with
//! `SetWindowPos` causes N separate composition frames and visible jitter
//! during a tile pass; deferring N moves into one HDWP makes them appear in
//! the same composition frame.
//!
//! Critical flags:
//! * `SWP_NOACTIVATE` — don't steal foreground when moving non-focused tiles
//! * `SWP_NOZORDER`   — keep current Z order; layout doesn't reshuffle stacks
//! * `SWP_NOREDRAW`   — don't trigger WM_PAINT during the defer (DWM redraws
//!   atomically when EndDeferWindowPos commits the batch)
//! * `SWP_FRAMECHANGED` — recompute non-client area; needed when geometry
//!   changes substantially or styles were edited.

use crate::Rect;
use crate::Result;
use color_eyre::eyre::eyre;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, HDWP, IsZoomed, SW_RESTORE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOZORDER, SetWindowPos, ShowWindow,
};

/// Standard flag set used for every batched DWMend move.
pub const STD_FLAGS: u32 = SWP_NOACTIVATE.0 | SWP_NOZORDER.0 | SWP_NOREDRAW.0 | SWP_FRAMECHANGED.0;

/// Apply the entire set of (window, target rect) moves in a single atomic
/// pass. Returns `Ok(())` if all moves were queued successfully, or `Err` if
/// the OS failed to allocate the HDWP — in which case callers may want to
/// fall back to one-by-one `set_position`.
pub fn apply_positions(moves: &[(HWND, Rect)]) -> Result<()> {
    if moves.is_empty() {
        return Ok(());
    }

    // Pre-pass: synchronously restore any maximised windows. SetWindowPos is
    // silently ignored on a WS_MAXIMIZE window (it stores the new rect as
    // the "restored" rect but the visible rect stays full-screen), so
    // without this pre-pass the user's window stays expanded after toggling
    // monocle off or moving a manually-maximised window into a tile slot.
    // ShowWindow can briefly block on a hung app but that is the price for
    // guaranteed geometry on the next move.
    for &(hwnd, _) in moves {
        // SAFETY: IsZoomed is safe with any HWND.
        if unsafe { IsZoomed(hwnd) }.as_bool() {
            // SAFETY: ShowWindow is safe with any HWND; SW_RESTORE is a valid command.
            let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        }
    }

    // SAFETY: nNumWindows >= 1 (we early-returned on empty).
    let mut hdwp: HDWP = unsafe { BeginDeferWindowPos(moves.len() as i32) }
        .map_err(|e| eyre!("BeginDeferWindowPos({}) failed: {e}", moves.len()))?;

    for &(hwnd, r) in moves {
        // SAFETY: hdwp valid (just allocated / re-returned); hwnd is opaque
        // OS handle, SetWindowPos-style flag set is valid.
        match unsafe {
            DeferWindowPos(
                hdwp,
                hwnd,
                None,
                r.x,
                r.y,
                r.w,
                r.h,
                windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(STD_FLAGS),
            )
        } {
            Ok(new) => hdwp = new,
            Err(e) => {
                // One failed move shouldn't abort the whole tile pass — log
                // and continue with the rest. If hdwp is now invalid, the
                // EndDeferWindowPos below will fail and we'll surface that.
                tracing::warn!(?hwnd, error = %e, "DeferWindowPos failed; continuing");
            }
        }
    }

    // SAFETY: hdwp valid; EndDeferWindowPos consumes it.
    unsafe {
        EndDeferWindowPos(hdwp).map_err(|e| eyre!("EndDeferWindowPos failed: {e}"))?;
    }
    Ok(())
}

/// One-off move for cases where `apply_positions` is overkill (drag-end
/// snap-to-tile, single floating-window placement, etc.). Uses the same flag
/// set so behaviour matches the batch path.
pub fn set_position(hwnd: HWND, r: Rect) -> Result<()> {
    // SAFETY: SetWindowPos is safe with any HWND; insert-after = None.
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            r.x,
            r.y,
            r.w,
            r.h,
            windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(STD_FLAGS),
        )
        .map_err(|e| eyre!("SetWindowPos failed: {e}"))
    }
}
