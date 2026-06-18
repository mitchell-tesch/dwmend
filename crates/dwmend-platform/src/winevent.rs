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
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
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

/// `true` while the listener thread is running. Set on thread entry and
/// cleared by an RAII guard on exit (including panic). Polled by the
/// daemon's supervisor tick to detect silent thread death.
static LISTENER_ALIVE: AtomicBool = AtomicBool::new(false);

// ---- public API -------------------------------------------------------------

/// Spawn the WinEvent listener thread. Safe to call only once per process —
/// subsequent calls return an error.
pub fn start() -> Result<Receiver<WinEvent>> {
    let (tx, rx) = unbounded();

    if EVENT_TX.set(tx).is_err() {
        return Err(eyre!("winevent::start called more than once"));
    }

    spawn_thread()?;
    Ok(rx)
}

/// Whether the listener thread is currently running. Used by the daemon's
/// supervisor to decide whether [`restart`] is needed.
pub fn is_alive() -> bool {
    LISTENER_ALIVE.load(Ordering::SeqCst)
}

/// Respawn the listener thread on the existing channel. Intended for the
/// daemon's watchdog after observing [`is_alive`] return `false`. Errors if
/// `start` was never called or if the thread is still alive.
pub fn restart() -> Result<()> {
    if EVENT_TX.get().is_none() {
        return Err(eyre!("winevent::restart called before start"));
    }
    if LISTENER_ALIVE.load(Ordering::SeqCst) {
        return Err(eyre!("winevent listener still alive; refusing to restart"));
    }
    spawn_thread()
}

fn spawn_thread() -> Result<()> {
    // Set ALIVE before spawn to close the race where a supervisor tick
    // sees `false` between our spawn call and the new thread reaching its
    // RAII guard. The thread will set it again on entry; the guard clears
    // it on exit. If `spawn` itself fails we restore `false` here.
    LISTENER_ALIVE.store(true, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("dwmend-winevent".into())
        .spawn(run_listener_thread)
        .map_err(|e| {
            LISTENER_ALIVE.store(false, Ordering::SeqCst);
            eyre!("failed to spawn dwmend-winevent thread: {e}")
        })?;
    Ok(())
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
    // RAII guard: clears LISTENER_ALIVE (and TID/HOOK if still set) on any
    // exit path, including panics. The supervisor in daemon.rs polls
    // `is_alive()` and respawns when this flips to false, so a clean
    // teardown signal here is the supervisor's only fault-detection input.
    struct AliveGuard;
    impl Drop for AliveGuard {
        fn drop(&mut self) {
            // Best-effort unhook in case we panicked mid-pump.
            let h = HOOK.swap(0, Ordering::SeqCst);
            if h != 0 {
                // SAFETY: handle was produced by a successful
                // SetWinEventHook earlier; UnhookWinEvent on a stale handle
                // is documented as a no-op returning FALSE.
                let _ = unsafe { UnhookWinEvent(HWINEVENTHOOK(h as *mut _)) };
            }
            LISTENER_TID.store(0, Ordering::SeqCst);
            LISTENER_ALIVE.store(false, Ordering::SeqCst);
        }
    }
    let _alive = AliveGuard;
    LISTENER_ALIVE.store(true, Ordering::SeqCst);

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

    // Normal cleanup. The AliveGuard handles the panic path; doing it
    // here too keeps the unhook eager when the message loop exits cleanly.
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
        // EVENT_OBJECT_LOCATIONCHANGE is dropped at the source: it fires
        // tens-to-hundreds of times per second during mouse hovers, animations,
        // and caret blinks, the daemon has no actionable response for it,
        // and forwarding it would still cost a channel hop + WM mutex acquire
        // per event. The `WinEvent::LocationChanged` variant is retained for
        // forward-compatibility (e.g. a future drag-to-tile feature would
        // re-enable this arm with a managed-HWND filter).
        EVENT_OBJECT_LOCATIONCHANGE => return,
        EVENT_OBJECT_NAMECHANGE => WinEvent::NameChanged(hwnd_isize),
        _ => return, // ignore everything else
    };

    if let Some(tx) = EVENT_TX.get() {
        // `try_send` on an unbounded channel never blocks; the only failure
        // mode is a disconnected receiver, which means we're shutting down.
        let _ = tx.try_send(mapped);
    }
}
