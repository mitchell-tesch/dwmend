//! Initial top-level window enumeration via `EnumWindows`.
//!
//! Returns every visible top-level window as a bare `Window` — callers should
//! apply manageability filtering separately (the platform layer is
//! intentionally policy-free).

use crate::Result;
use crate::window::Window;
use color_eyre::eyre::eyre;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
use windows::core::BOOL;

/// Enumerate every top-level window in the current desktop.
pub fn enumerate_top_level() -> Result<Vec<Window>> {
    let mut sink: Box<Vec<Window>> = Box::default();
    let lparam = LPARAM(std::ptr::from_mut(sink.as_mut()) as isize);

    // SAFETY: callback signature matches; sink Vec lives until EnumWindows returns.
    unsafe {
        EnumWindows(Some(enum_proc), lparam).map_err(|e| eyre!("EnumWindows failed: {e}"))?;
    }

    Ok(*sink)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam was constructed in `enumerate_top_level` as a pointer to
    // a heap-allocated Vec<Window>.
    let sink = unsafe { &mut *(lparam.0 as *mut Vec<Window>) };
    sink.push(Window::from_hwnd(hwnd));
    BOOL(1) // continue
}
