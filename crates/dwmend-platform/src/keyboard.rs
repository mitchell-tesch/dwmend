//! Global hotkeys via `RegisterHotKey`.
//!
//! ## Why not WH_KEYBOARD_LL
//!
//! A previous iteration used a low-level keyboard hook. The hook works, but
//! Windows enforces a strict `LowLevelHooksTimeout` (~300 ms). If the hook
//! thread's message pump is delayed even briefly — by file rotation in
//! `tracing-appender`, by lock contention, or by COM activation in another
//! subsystem — Windows quietly removes the hook *and* throttles input
//! system-wide for the duration of the timeout. That made the daemon look
//! "alive but unresponsive" with no clear failure mode in the log.
//!
//! `RegisterHotKey` avoids all of that:
//! * No global hook injected into other processes
//! * No callback budget; events arrive as ordinary `WM_HOTKEY` messages
//! * The OS resolves modifier state for us
//! * Built-in conflict detection (returns an Err if the combo is taken)
//!
//! Tradeoff: a few combos are reserved by the OS (`SUPER+L` = lock,
//! `SUPER+TAB` = task view, etc.). Those registrations fail and we log a
//! warning rather than aborting.
//!
//! ## Threading model
//!
//! One dedicated thread:
//!   1. Creates a message-only window
//!   2. Iterates the table and calls `RegisterHotKey` for each entry
//!   3. Pumps `GetMessage` and forwards `WM_HOTKEY` events to a channel

use crate::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostThreadMessageW, RegisterClassExW, TranslateMessage, WINDOW_EX_STYLE, WM_DESTROY,
    WM_HOTKEY, WM_QUIT, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::PCWSTR;

bitflags::bitflags! {
    /// Modifier bitmask used as part of the hotkey lookup key.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Mods: u8 {
        const SUPER = 0b0001;
        const CTRL  = 0b0010;
        const ALT   = 0b0100;
        const SHIFT = 0b1000;
    }
}

impl Mods {
    fn to_win32(self) -> HOT_KEY_MODIFIERS {
        // MOD_NOREPEAT: skip auto-repeat events while the key is held down.
        // Without this, holding `SUPER+H` for half a second would fire focus-left
        // a dozen times.
        let mut m = MOD_NOREPEAT;
        if self.contains(Mods::SUPER) {
            m |= MOD_WIN;
        }
        if self.contains(Mods::CTRL) {
            m |= MOD_CONTROL;
        }
        if self.contains(Mods::ALT) {
            m |= MOD_ALT;
        }
        if self.contains(Mods::SHIFT) {
            m |= MOD_SHIFT;
        }
        m
    }
}

/// A single binding entry: `((Mods, vk), HotkeyId)`. The `HotkeyId` is an
/// opaque integer the host crate maps back to a `Command`.
pub type HotkeyTable = HashMap<(Mods, u16), HotkeyId>;

/// Opaque integer identifying a binding. Owned and assigned by the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HotkeyId(pub u32);

/// Message published when a registered hotkey fires.
#[derive(Debug, Clone, Copy)]
pub struct HotkeyMatch {
    pub id: HotkeyId,
    pub mods: Mods,
    pub vk: u16,
}

// ---- shared state ----------------------------------------------------------

static EVENT_TX: OnceLock<Sender<HotkeyMatch>> = OnceLock::new();
static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);

/// When true, the host-side dispatcher should ignore non-toggle commands.
/// (We don't suppress the WM_HOTKEY itself; the host crate decides what to
/// act on. This flag exists as a coordination point with the rest of DWMend.)
pub static PAUSED: AtomicBool = AtomicBool::new(false);

// ---- public API -------------------------------------------------------------

/// Install all hotkeys in `table` on a dedicated thread.
///
/// Returns a `Receiver<HotkeyMatch>` for incoming events. The thread owns
/// the registrations for its lifetime; calling [`stop`] cleanly unregisters
/// them. May be called only once per process.
pub fn start(table: HotkeyTable) -> Result<Receiver<HotkeyMatch>> {
    let (tx, rx) = unbounded();
    if EVENT_TX.set(tx).is_err() {
        return Err(eyre!("keyboard::start called more than once"));
    }

    std::thread::Builder::new()
        .name("dwmend-hotkeys".into())
        .spawn(move || run_listener_thread(table))
        .map_err(|e| eyre!("failed to spawn dwmend-hotkeys thread: {e}"))?;

    Ok(rx)
}

/// Cooperative shutdown — post WM_QUIT to the hotkey thread.
pub fn stop() {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    // SAFETY: posting to a thread ID is always safe.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

// ---- listener thread --------------------------------------------------------

fn run_listener_thread(table: HotkeyTable) {
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    let Some(hwnd) = create_message_window() else {
        tracing::error!("hotkey listener: could not create message window");
        return;
    };

    // Register each hotkey against this window. Windows uses an i32 ID space
    // 0..=0xBFFF for application-level hotkeys; we just monotonically allocate.
    // Map win_id -> (our_id, mods, vk) so WM_HOTKEY can be reverse-mapped.
    let mut index: HashMap<i32, (HotkeyId, Mods, u16)> = HashMap::with_capacity(table.len());
    let mut conflicts: Vec<String> = Vec::new();
    for (win_id, ((mods, vk), our_id)) in (1_i32..).zip(table.iter()) {
        let modifiers = mods.to_win32();
        // SAFETY: RegisterHotKey is safe with a valid hwnd; vk and modifiers
        // are simple integer parameters.
        match unsafe { RegisterHotKey(Some(hwnd), win_id, modifiers, *vk as u32) } {
            Ok(()) => {
                index.insert(win_id, (*our_id, *mods, *vk));
            }
            Err(e) => {
                conflicts.push(format!("{mods:?}+{:#x}: {e}", *vk));
            }
        }
    }
    if !conflicts.is_empty() {
        tracing::warn!(
            count = conflicts.len(),
            "some hotkeys failed to register (likely reserved by Windows or another app)"
        );
        for c in &conflicts {
            tracing::warn!("  - {c}");
        }
    }
    tracing::info!(
        registered = index.len(),
        failed = conflicts.len(),
        tid = tid,
        "hotkey listener started"
    );

    // Message pump. WM_HOTKEY arrives here for each registered combo.
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param; restricting to our hwnd avoids
        // chewing through unrelated thread messages.
        let r = unsafe { GetMessageW(&mut msg, Some(hwnd), 0, 0) };
        if r.0 <= 0 {
            // WM_QUIT (0) or error (-1).
            break;
        }
        if msg.message == WM_HOTKEY {
            let win_id = msg.wParam.0 as i32;
            if let Some(&(our_id, mods, vk)) = index.get(&win_id)
                && let Some(tx) = EVENT_TX.get()
            {
                let _ = tx.try_send(HotkeyMatch {
                    id: our_id,
                    mods,
                    vk,
                });
            }
            continue;
        }
        // SAFETY: msg is populated.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup.
    for &win_id in index.keys() {
        // SAFETY: hwnd valid; win_id was successfully registered above.
        let _ = unsafe { UnregisterHotKey(Some(hwnd), win_id) };
    }
    // SAFETY: hwnd valid.
    let _ = unsafe { DestroyWindow(hwnd) };
    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("hotkey listener exited");
}

fn create_message_window() -> Option<HWND> {
    let class_name = utf16(b"DwmendHotkeyListener\0");

    // SAFETY: GetModuleHandleW(None) always returns the current EXE.
    let hinst = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => {
            tracing::error!(error = %e, "GetModuleHandleW failed");
            return None;
        }
    };

    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: wnd_class is fully initialised; class_name is a null-terminated wstr.
    let atom = unsafe { RegisterClassExW(&wnd_class) };
    if atom == 0 {
        tracing::error!("RegisterClassExW failed for hotkey listener");
        return None;
    }

    // SAFETY: parameters are valid; PCWSTR pointers outlive the call.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None, // hMenu
            Some(hinst),
            None,
        )
    };
    match hwnd {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!(error = %e, "CreateWindowExW failed for hotkey listener");
            None
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => LRESULT(0),
        // SAFETY: DefWindowProcW is the documented fallback handler.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Convert a byte literal ending in `\0` to a Vec<u16> null-terminated wstr.
fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}
