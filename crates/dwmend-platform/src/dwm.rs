//! DWM (Desktop Window Manager) attribute helpers.
//!
//! Three things DWMend gets from DWM:
//! * `DWMWA_TRANSITIONS_FORCEDISABLED` — set on every managed window so
//!   the OS does not animate slide/fade on reposition. Without this, every
//!   batched tiling pass looks like a slot-machine.
//! * `DWMWA_CLOAKED` (read) — tells us whether the Shell or inheritance
//!   has cloaked a window. Such windows live on other Virtual Desktops we
//!   are not managing. (DWMend hides its own windows via `ShowWindowAsync` —
//!   see `window::Window::cloak` for why DWMWA_CLOAK Set is no longer used.)
//! * `DWMWA_EXTENDED_FRAME_BOUNDS` (read) — the visual frame rectangle
//!   without the invisible drop-shadow / resize margins `GetWindowRect`
//!   includes. Used so the focus overlay hugs the window symmetrically.

use crate::Result;
use color_eyre::eyre::eyre;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_TRANSITIONS_FORCEDISABLED,
    DwmGetWindowAttribute, DwmSetWindowAttribute,
};
use windows::core::BOOL;

/// `DWMWA_CLOAKED` flag values (defined in dwmapi.h but not always re-exported).
pub const DWM_CLOAKED_SHELL: u32 = 0x0000_0002;
pub const DWM_CLOAKED_INHERITED: u32 = 0x0000_0004;

/// Turn off OS-level transition animations for `hwnd`. Idempotent.
pub fn disable_transitions(hwnd: HWND) -> Result<()> {
    let value: BOOL = true.into();
    // SAFETY: pvAttribute points to a BOOL on our stack with cbAttribute set
    // correctly; DwmSetWindowAttribute is a synchronous, thread-safe call.
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            std::ptr::from_ref(&value).cast(),
            std::mem::size_of::<BOOL>() as u32,
        )
        .map_err(|e| eyre!("DwmSetWindowAttribute(TRANSITIONS_FORCEDISABLED) failed: {e}"))
    }
}

/// Read the `DWMWA_CLOAKED` bitmask. Returns 0 if not cloaked.
fn cloaked_reason(hwnd: HWND) -> Result<u32> {
    let mut value: u32 = 0;
    // SAFETY: pvAttribute points to a u32 on our stack matching cbAttribute.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            std::ptr::from_mut(&mut value).cast(),
            std::mem::size_of::<u32>() as u32,
        )
        .map_err(|e| eyre!("DwmGetWindowAttribute(CLOAKED) failed: {e}"))?;
    }
    Ok(value)
}

/// True iff the cloak came from the Shell or was inherited — i.e. the window
/// is on another Virtual Desktop. Such windows must be ignored entirely.
pub fn is_cloaked_by_shell(hwnd: HWND) -> bool {
    cloaked_reason(hwnd)
        .map(|v| v & (DWM_CLOAKED_SHELL | DWM_CLOAKED_INHERITED) != 0)
        .unwrap_or(false)
}

/// Pack an `0xRRGGBB` triple into a Win32 `COLORREF` (`0x00BBGGRR`).
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Return the window's *visual* frame bounds via `DWMWA_EXTENDED_FRAME_BOUNDS`.
///
/// `GetWindowRect` reports the full window rectangle, which on Windows 10+
/// includes the invisible "resize handle" / drop-shadow margins DWM adds
/// outside the visible chrome. Those margins are asymmetric — typically
/// ~7 px on the left/right/bottom and ~1 px at the top — so anything that
/// uses `GetWindowRect` to place a frame around the window ends up with a
/// visible gap on three sides and a tight fit on top.
///
/// `DWMWA_EXTENDED_FRAME_BOUNDS` returns the visual rect, which is what
/// the user actually sees and what the focus overlay should hug.
pub fn extended_frame_bounds(hwnd: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    // SAFETY: rect is a valid out-param; cbAttribute matches its size.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            std::ptr::from_mut(&mut rect).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
        .map_err(|e| eyre!("DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS) failed: {e}"))?;
    }
    Ok(rect)
}
