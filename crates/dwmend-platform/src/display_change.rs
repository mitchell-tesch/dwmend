//! Hidden top-level window that translates `WM_DISPLAYCHANGE` and
//! `WM_SETTINGCHANGE` into events on the main channel, so a monitor hot-plug
//! or DPI change triggers a re-enumerate.
//!
//! Why a top-level window (and not a message-only HWND_MESSAGE window)?
//! Per Microsoft docs, `WM_DISPLAYCHANGE` is "only sent to top-level windows"
//! and message-only windows "do not receive broadcast messages" — that
//! includes `WM_SETTINGCHANGE` (SPI_SETWORKAREA). An HWND_MESSAGE-parented
//! listener silently swallows every monitor hot-plug, so the daemon never
//! re-tiles after an unplug and the apps stay sized for the dead monitor.
//!
//! To stay invisible without becoming message-only we create a 0×0 top-level
//! window with `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` and never set
//! `WS_VISIBLE`. Tool-window keeps it out of Alt-Tab and out of every
//! reasonable tiling WM's manage list (including our own `filter.rs`).

use crate::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Receiver, Sender, unbounded};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
    RegisterClassExW, TranslateMessage, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED,
    WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use windows::core::PCWSTR;

#[derive(Debug, Clone, Copy)]
pub enum DisplayEvent {
    /// Display topology changed — count, resolution, or DPI moved.
    TopologyChanged,
}

static EVENT_TX: std::sync::OnceLock<Sender<DisplayEvent>> = std::sync::OnceLock::new();

/// Spawn the listener thread. Returns a receiver of `DisplayEvent`s.
pub fn start() -> Result<Receiver<DisplayEvent>> {
    let (tx, rx) = unbounded();
    if EVENT_TX.set(tx).is_err() {
        return Ok(rx);
    }

    std::thread::Builder::new()
        .name("dwmend-display".into())
        .spawn(run_thread)
        .map_err(|e| eyre!("failed to spawn dwmend-display thread: {e}"))?;

    Ok(rx)
}

fn run_thread() {
    let class_name = utf16(b"DwmendDisplayListener\0");

    // SAFETY: GetModuleHandleW(None) always returns the current EXE.
    let hinst = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => {
            tracing::error!(error = %e, "GetModuleHandleW failed; display listener disabled");
            return;
        }
    };

    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };

    // SAFETY: wnd_class fully initialized; class_name is a valid null-terminated wstr.
    let atom = unsafe { RegisterClassExW(&wnd_class) };
    if atom == 0 {
        tracing::error!("RegisterClassExW failed for display listener");
        return;
    }

    // Create a 0×0 hidden top-level window. We deliberately do NOT use
    // HWND_MESSAGE: WM_DISPLAYCHANGE and broadcast WM_SETTINGCHANGE skip
    // message-only windows entirely. WS_EX_TOOLWINDOW + WS_EX_NOACTIVATE
    // keep this helper out of Alt-Tab and out of our own manageability
    // filter (see `filter.rs`).
    // SAFETY: parameters are valid; PCWSTR pointers outlive the call.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None, // hWndParent
            None, // hMenu
            Some(hinst),
            None,
        )
    };

    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "CreateWindowExW failed for display listener");
            return;
        }
    };

    tracing::info!("display-change listener started");

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param. We pump the entire thread queue
        // (hwnd = None) rather than filtering to our hwnd, because broadcast
        // WM_SETTINGCHANGE arrives via SendNotifyMessage and may be posted
        // to the thread queue rather than addressed to a specific hwnd.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        // SAFETY: msg populated.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // SAFETY: destroy is safe with a valid hwnd; failure is just logged.
    let _ = unsafe { DestroyWindow(hwnd) };
    tracing::info!("display-change listener exited");
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            if let Some(tx) = EVENT_TX.get() {
                let _ = tx.try_send(DisplayEvent::TopologyChanged);
            }
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // wparam == SPI_SETWORKAREA (0x002F) signals taskbar / dock changes.
            if wparam.0 as u32 == 0x002F
                && let Some(tx) = EVENT_TX.get()
            {
                let _ = tx.try_send(DisplayEvent::TopologyChanged);
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        // SAFETY: DefWindowProcW is the documented fallback handler.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Convert a byte literal ending in `\0` to a Vec<u16> null-terminated wstr.
fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}
