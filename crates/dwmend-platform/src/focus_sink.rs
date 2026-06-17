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
//!
//! ## Threading
//!
//! The sink is a top-level `WS_POPUP` window, which means the OS will route
//! synchronous broadcast `SendMessage`s at it (`WM_SETTINGCHANGE`,
//! `WM_DEVICECHANGE`, `WM_THEMECHANGED`, `WM_DISPLAYCHANGE`,
//! `WM_POWERBROADCAST`, `WM_TIMECHANGE`, internal "is alive" probes, …).
//! Each of those calls blocks the sender until our wnd_proc runs — and the
//! wnd_proc only runs when the **owning thread** pumps messages. So the
//! sink lives on a dedicated `dwmend-focus-sink` thread that does nothing
//! but `GetMessage` / `DispatchMessage`. Without this, the daemon's main
//! thread (which runs `crossbeam_channel::select!` instead of a Win32
//! pump) would stall those broadcasts and Windows would pop the
//! "this application is not responding" dialog after ~5 seconds.
//!
//! `take_focus()` calls `SetForegroundWindow` on the cached HWND from any
//! thread; that's documented to be process-scoped, not thread-scoped, so
//! cross-thread invocation is safe.

use crate::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Sender, bounded};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
    PostThreadMessageW, RegisterClassExW, SW_SHOWNOACTIVATE, SetForegroundWindow, ShowWindow,
    TranslateMessage, WINDOW_EX_STYLE, WM_QUIT, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::PCWSTR;

static SINK: OnceLock<isize> = OnceLock::new();
static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);

/// Spawn the focus-sink thread. Safe to call once per process — subsequent
/// calls are no-ops. The thread creates a 1×1 off-screen `WS_POPUP` window,
/// shows it (so `SetForegroundWindow` accepts it as a target), and then
/// pumps messages for its entire lifetime.
pub fn start() -> Result<()> {
    if SINK.get().is_some() {
        return Ok(());
    }

    let (init_tx, init_rx) = bounded::<std::result::Result<isize, String>>(1);

    std::thread::Builder::new()
        .name("dwmend-focus-sink".into())
        .spawn(move || run_sink_thread(init_tx))
        .map_err(|e| eyre!("spawn dwmend-focus-sink thread: {e}"))?;

    // Block until the thread reports the HWND (or fails). Without the
    // handshake, `take_focus()` could fire before the HWND was ready and
    // silently no-op on the very first workspace switch.
    match init_rx
        .recv()
        .map_err(|_| eyre!("focus sink thread died during init"))?
    {
        Ok(hwnd_isize) => {
            let _ = SINK.set(hwnd_isize);
            tracing::info!(hwnd = format!("{hwnd_isize:#x}"), "focus sink ready");
            Ok(())
        }
        Err(e) => Err(eyre!("focus sink init failed: {e}")),
    }
}

/// Cooperative shutdown — post `WM_QUIT` to the sink thread so it can
/// destroy the window and exit cleanly. Idempotent.
pub fn stop() {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    // SAFETY: posting to a thread ID is always safe; if the thread already
    // exited the post fails silently which is fine.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

/// Push OS foreground to the sink so the previously-focused (now cloaked
/// or hidden) window stops receiving keystrokes. Best-effort: if the
/// foreground-stealing rules deny us, the cloaked window keeps focus — a
/// log line records it.
pub fn take_focus() {
    let Some(&h) = SINK.get() else { return };
    // SAFETY: HWND from our own CreateWindow. SetForegroundWindow is
    // documented as cross-thread safe (it only enforces *process* lock).
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

// ---- listener thread --------------------------------------------------------

fn run_sink_thread(init_tx: Sender<std::result::Result<isize, String>>) {
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    let hwnd = match create_sink_window() {
        Ok(h) => h,
        Err(msg) => {
            let _ = init_tx.send(Err(msg));
            LISTENER_TID.store(0, Ordering::SeqCst);
            return;
        }
    };

    // Publish the HWND BEFORE entering the pump so `start()` can return and
    // callers can immediately call `take_focus()`.
    let _ = init_tx.send(Ok(hwnd.0 as isize));

    // Standard message pump. Every broadcast `SendMessage` aimed at top-
    // level windows lands here and is dispatched to `wnd_proc` (which just
    // forwards to `DefWindowProcW`). The OS sees a responsive window and
    // never raises the "not responding" prompt.
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param; hwnd=None means all messages
        // delivered to this thread (including thread-targeted WM_QUIT).
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // BOOL: 0 = WM_QUIT, -1 = error, else continue.
        if r.0 <= 0 {
            break;
        }
        // SAFETY: msg is populated.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // SAFETY: hwnd was returned by CreateWindowExW and has not been destroyed
    // elsewhere — the sink thread is the only owner.
    let _ = unsafe { DestroyWindow(hwnd) };
    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("focus sink thread exited");
}

fn create_sink_window() -> std::result::Result<HWND, String> {
    let class_name = utf16(b"DwmendFocusSink\0");

    // SAFETY: GetModuleHandleW(None) returns the current EXE.
    let hinst = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => return Err(format!("GetModuleHandleW: {e}")),
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
    .map_err(|e| format!("CreateWindowExW (focus sink): {e}"))?;

    // CRITICAL: SetForegroundWindow refuses HIDDEN windows. Show the sink
    // without activating it so it's eligible as a foreground target while
    // staying off-screen (and out of Alt-Tab thanks to WS_EX_TOOLWINDOW).
    // Without this, take_focus() silently no-ops and the previously
    // cloaked managed window keeps OS foreground / keystrokes.
    // SAFETY: HWND from CreateWindowExW above.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };

    Ok(hwnd)
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
