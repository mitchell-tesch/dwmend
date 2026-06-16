//! Hidden focus sink — a tiny off-screen top-level window used as a focus
//! target when no managed window should receive input.
//!
//! ## Why this is needed
//!
//! When DWMend switches to a workspace with no windows, the previously-focused
//! application would otherwise stay the OS foreground window (hiding a
//! window via `ShowWindowAsync(SW_HIDE)` does NOT change foreground).
//! Typed keystrokes would silently go to the hidden window — including
//! destructive ones like Ctrl+W, Enter, etc.
//!
//! Foregrounding this 1×1 invisible window kicks the hidden window out of
//! the foreground role. The user sees nothing because the sink is off-screen
//! and has no client area to paint.
//!
//! ## Why a dedicated window (not the desktop / a bar)
//!
//! * The status bar uses `WS_EX_NOACTIVATE` so it cannot become foreground.
//! * `GetDesktopWindow()` returns the root desktop which can't be foregrounded
//!   either.
//! * Foregrounding Progman (Explorer's desktop window) works but visibly
//!   shifts the user's notion of "what's active" to Explorer.
//!
//! A dedicated invisible window is the cleanest "/dev/null" for focus.

use crate::Result;
use color_eyre::eyre::eyre;
use std::sync::OnceLock;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassExW, SW_SHOWNOACTIVATE, SetForegroundWindow,
    ShowWindow, WINDOW_EX_STYLE, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::PCWSTR;

static SINK: OnceLock<isize> = OnceLock::new();

/// Create the sink window on the calling thread. Idempotent (subsequent
/// calls are no-ops). The sink has no message pump of its own — input
/// directed at it queues up and is silently dropped, which is precisely
/// what we want.
pub fn start() -> Result<()> {
    if SINK.get().is_some() {
        return Ok(());
    }

    let class_name = utf16(b"DwmendFocusSink\0");

    // SAFETY: GetModuleHandleW(None) returns the current EXE.
    let hinst = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => return Err(eyre!("GetModuleHandleW: {e}")),
    };

    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: class is fully initialised; name is null-terminated.
    // An atom of 0 here may mean the class is already registered (idempotent
    // on subsequent process spawns inside one explorer.exe session); we treat
    // that as success and rely on CreateWindowExW below to surface real errors.
    let _atom = unsafe { RegisterClassExW(&class) };

    // No WS_EX_NOACTIVATE: we WANT SetForegroundWindow to succeed on this.
    let ex = WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0);

    // SAFETY: parameters are valid; PCWSTR pointers outlive the call.
    // Position the window deep off-screen so it's invisible even if the user
    // somehow Alt-Tabs to it. WS_POPUP + no parent => top-level window.
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WS_POPUP,
            -32000,
            -32000,
            1,
            1,
            None, // hWndParent
            None, // hMenu
            Some(hinst),
            None,
        )
    }
    .map_err(|e| eyre!("CreateWindowExW (focus sink): {e}"))?;

    // CRITICAL: SetForegroundWindow refuses HIDDEN windows. Show the sink
    // without activating it so it's eligible as a foreground target while
    // staying off-screen (and out of Alt-Tab thanks to WS_EX_TOOLWINDOW).
    // Without this, take_focus() silently no-ops and the previously
    // cloaked managed window keeps OS foreground / keystrokes.
    // SAFETY: HWND from CreateWindowExW above.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };

    let _ = SINK.set(hwnd.0 as isize);
    tracing::info!(hwnd = format!("{:#x}", hwnd.0 as isize), "focus sink ready");
    Ok(())
}

/// Push OS foreground to the sink so the previously-focused (now cloaked
/// or hidden) window stops receiving keystrokes. Best-effort: if the
/// foreground-stealing rules deny us, the cloaked window keeps focus — a
/// log line records it.
pub fn take_focus() {
    let Some(&h) = SINK.get() else { return };
    // SAFETY: HWND from our own CreateWindow.
    let ok = unsafe { SetForegroundWindow(HWND(h as *mut _)) };
    if ok.as_bool() {
        tracing::debug!(hwnd = format!("{:#x}", h), "focus sink: took foreground");
    } else {
        tracing::warn!(
            hwnd = format!("{:#x}", h),
            "focus sink: SetForegroundWindow denied (foreground-lock); cloaked window may keep keystrokes"
        );
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: DefWindowProcW is the documented fallback handler.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}
