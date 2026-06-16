//! WinEvent hook on a dedicated message-loop thread.
//!
//! Threading model: ONE thread, that thread owns:
//!   1. the `SetWinEventHook` registration
//!   2. the `GetMessage` / `DispatchMessage` pump (required: the callback only
//!      fires while messages are being processed)
//!   3. a `WM_QUIT` listener for cooperative shutdown
//!
//! Out-of-context delivery is fine for us — we don't need the lowest latency
//! and we do need the cross-process events anyway.

use crate::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_MAX, EVENT_MIN, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE,
    EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GetMessageW,
    MSG, OBJID_WINDOW, PostThreadMessageW, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_QUIT,
};

/// A WinEvent we care about, normalized into a small enum the rest of DWMend
/// can match on without depending on `windows::*` everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WinEvent {
    /// Window became visible (created, restored from minimize, etc.). Subject
    /// to our `filter::is_manageable` check before we manage.
    Shown(isize),
    /// Window is going away. Drop from state immediately.
    Destroyed(isize),
    /// Temporarily hidden (likely SW_HIDE). Keep in state but mark.
    Hidden(isize),
    /// DWM cloak flag set by an external party (not us).
    Cloaked(isize),
    /// DWM cloak cleared.
    Uncloaked(isize),
    /// Foreground window changed.
    Foreground(isize),
    /// User started dragging or resizing with the mouse.
    MoveSizeStart(isize),
    /// User finished dragging or resizing. Useful for "snap to BSP slot".
    MoveSizeEnd(isize),
    /// Window minimized (manually by user, or programmatically).
    Minimized(isize),
    /// Window restored from minimized state.
    Restored(isize),
    /// Window geometry changed (used to detect maximize/restore round-trips).
    LocationChanged(isize),
    /// Window title changed — useful for refreshing rule matches.
    NameChanged(isize),
}

// ---- module state -----------------------------------------------------------

/// The sender used by the hook callback to publish events. Set once when the
/// listener thread starts; never reset.
static EVENT_TX: OnceLock<Sender<WinEvent>> = OnceLock::new();

/// Thread ID of the listener thread; used by `stop()` to post WM_QUIT.
static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);

/// Registered hook handle; used by `stop()` to unhook.
static HOOK: AtomicIsize = AtomicIsize::new(0);

// ---- public API -------------------------------------------------------------

/// Spawn the WinEvent listener thread. Safe to call only once per process —
/// subsequent calls return the existing receiver.
pub fn start() -> Result<Receiver<WinEvent>> {
    let (tx, rx) = unbounded();

    // First caller wins; later callers just get a fresh receiver bound to the
    // original sender (whose channel is `unbounded` so it can fan out via clone).
    if EVENT_TX.set(tx.clone()).is_err() {
        return Ok(rx);
    }

    std::thread::Builder::new()
        .name("dwmend-winevent".into())
        .spawn(run_listener_thread)
        .map_err(|e| eyre!("failed to spawn dwmend-winevent thread: {e}"))?;

    Ok(rx)
}

/// Cooperatively stop the listener. Sends WM_QUIT to the listener thread,
/// which will exit its message loop, unhook, and return.
pub fn stop() {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    // SAFETY: posting WM_QUIT to a thread ID is always safe; if the thread
    // already exited the post fails silently which is fine.
    unsafe {
        let _ = PostThreadMessageW(
            tid,
            WM_QUIT,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
    }
}

// ---- listener thread --------------------------------------------------------

fn run_listener_thread() {
    // Record our thread ID so `stop()` can find us.
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    // Register the hook for the entire event range in one call (cheaper than
    // many narrow hooks).
    // SAFETY: callback has matching extern signature; flags are valid; idProcess
    // & idThread = 0 means all processes/threads on the current desktop.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_MIN,
            EVENT_MAX,
            None, // hmod = NULL for out-of-context
            Some(hook_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if hook.0.is_null() {
        tracing::error!("SetWinEventHook returned NULL; DWMend cannot observe windows");
        return;
    }
    HOOK.store(hook.0 as isize, Ordering::SeqCst);

    tracing::info!(?tid, "winevent listener thread started");

    // Message pump. GetMessage blocks until a message arrives; WM_QUIT (-1)
    // ends the loop. Out-of-context WinEvent callbacks fire while we are
    // inside GetMessage / DispatchMessage; without this loop the callback is
    // never invoked even though the hook is registered.
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param; hwnd=None means all messages.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // GetMessageW returns BOOL: 0 = WM_QUIT, -1 = error, else continue.
        if result.0 <= 0 {
            break;
        }
        // SAFETY: msg is initialized by GetMessageW.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup.
    // SAFETY: hook is valid (we stored it after a successful registration).
    let _ = unsafe { UnhookWinEvent(HWINEVENTHOOK(hook.0)) };
    HOOK.store(0, Ordering::SeqCst);
    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("winevent listener thread exited");
}

// ---- the actual hook callback ----------------------------------------------

/// Fired by the OS for every (event, hwnd) we asked about. MUST return fast
/// (queue + return). Anything heavy goes on the receiver thread.
unsafe extern "system" fn hook_callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    // We only care about top-level window events. OBJID_WINDOW == 0 — events
    // for other UI elements (menus, scrollbars, etc.) are noise.
    if id_object != OBJID_WINDOW.0 {
        return;
    }
    if hwnd.is_invalid() {
        return;
    }
    let hwnd_isize = hwnd.0 as isize;

    let mapped = match event {
        EVENT_OBJECT_SHOW | EVENT_OBJECT_CREATE => WinEvent::Shown(hwnd_isize),
        EVENT_OBJECT_DESTROY => WinEvent::Destroyed(hwnd_isize),
        EVENT_OBJECT_HIDE => WinEvent::Hidden(hwnd_isize),
        EVENT_OBJECT_CLOAKED => WinEvent::Cloaked(hwnd_isize),
        EVENT_OBJECT_UNCLOAKED => WinEvent::Uncloaked(hwnd_isize),
        EVENT_SYSTEM_FOREGROUND => WinEvent::Foreground(hwnd_isize),
        EVENT_SYSTEM_MOVESIZESTART => WinEvent::MoveSizeStart(hwnd_isize),
        EVENT_SYSTEM_MOVESIZEEND => WinEvent::MoveSizeEnd(hwnd_isize),
        EVENT_SYSTEM_MINIMIZESTART => WinEvent::Minimized(hwnd_isize),
        EVENT_SYSTEM_MINIMIZEEND => WinEvent::Restored(hwnd_isize),
        EVENT_OBJECT_LOCATIONCHANGE => WinEvent::LocationChanged(hwnd_isize),
        EVENT_OBJECT_NAMECHANGE => WinEvent::NameChanged(hwnd_isize),
        _ => return, // ignore everything else
    };

    if let Some(tx) = EVENT_TX.get() {
        // `try_send` on an unbounded channel never blocks; the only failure
        // mode is a disconnected receiver, which means we're shutting down.
        let _ = tx.try_send(mapped);
    }
}
