//! System-tray icon — `Shell_NotifyIcon` plus a right-click popup menu.
//!
//! ## Threading
//!
//! One dedicated thread owns:
//! * a hidden message-only window (so the icon's callback messages have
//!   somewhere to land);
//! * the `NIM_ADD` registration of the notify icon;
//! * a `GetMessage` pump that handles `WM_TRAY_CALLBACK` (right-click /
//!   double-click) by showing a `TrackPopupMenu`.
//!
//! Menu selections are pushed onto a `Receiver<TrayAction>` for the daemon
//! to convert into `Command`s on its own select loop. "Open config file" /
//! "Open log folder" are handled inline via `ShellExecuteW` because they
//! never need to touch DWMend state.
//!
//! ## Win11 visibility caveat
//!
//! On Windows 11 every newly-registered tray icon defaults to the chevron
//! overflow flyout. The user has to drag it out once for it to be pinned
//! visible. There is no programmatic way for a non-system app to change
//! that default — see the README.

use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, HWND_MESSAGE, IDI_APPLICATION, LoadIconW,
    MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostThreadMessageW, RegisterClassExW,
    SW_SHOWNORMAL, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_QUIT, WM_RBUTTONUP, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::PCWSTR;

/// Application-defined ID for our single tray icon entry. Must be unique per
/// HWND but we only ever own one icon, so a constant is fine.
const TRAY_ICON_ID: u32 = 1;

/// Resource ID of the application icon that `crates/dwmend/build.rs`
/// embeds into the EXE via `1 ICON "..\\..\\assets\\icon.ico"`. Loading
/// this is what gives the tray its dwindle/BSP-style glyph instead of the
/// generic Win32 default.
const IDI_DWMEND_APP: u16 = 1;

/// Callback message the OS posts to our window on tray mouse activity.
/// `WM_APP` is the documented base for app-private messages so it cannot
/// collide with anything else the OS sends.
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;

// Menu command IDs. Distinct from `WM_APP`-range to avoid confusion when
// reading logs.
const MENU_ID_PAUSE: usize = 100;
const MENU_ID_RELOAD: usize = 101;
const MENU_ID_OPEN_CONFIG: usize = 102;
const MENU_ID_OPEN_LOG_DIR: usize = 103;
const MENU_ID_QUIT: usize = 199;

/// What the daemon should do in response to a menu selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    TogglePause,
    ReloadConfig,
    Quit,
}

/// Caller-supplied paths the tray needs in order to handle "Open Config" /
/// "Open Log Folder" without round-tripping through the daemon.
#[derive(Debug, Clone)]
pub struct TrayConfig {
    pub config_path: PathBuf,
    pub log_dir: PathBuf,
}

// ---- module state -----------------------------------------------------------

static EVENT_TX: OnceLock<Sender<TrayAction>> = OnceLock::new();
static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);
static CONFIG_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static LOG_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

// ---- public API -------------------------------------------------------------

/// Spawn the tray thread and return a receiver for menu actions. Safe to
/// call at most once per process.
pub fn start(cfg: TrayConfig) -> Result<Receiver<TrayAction>> {
    let (tx, rx) = unbounded();
    if EVENT_TX.set(tx).is_err() {
        return Err(eyre!("tray::start called more than once"));
    }
    *CONFIG_PATH.lock() = Some(cfg.config_path);
    *LOG_DIR.lock() = Some(cfg.log_dir);

    std::thread::Builder::new()
        .name("dwmend-tray".into())
        .spawn(run_tray_thread)
        .map_err(|e| eyre!("failed to spawn dwmend-tray thread: {e}"))?;

    Ok(rx)
}

/// Update the "Pause"/"Resume" label shown in the menu. Called by the
/// daemon whenever the pause state flips so the next right-click reflects
/// reality.
pub fn set_paused(p: bool) {
    PAUSED.store(p, Ordering::SeqCst);
}

/// Cooperative shutdown — post WM_QUIT to the tray thread so it cleans up
/// the notify icon before exit. Idempotent.
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

// ---- listener thread --------------------------------------------------------

fn run_tray_thread() {
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    let Some((hwnd, hinst)) = create_message_window() else {
        tracing::error!("tray: failed to create message window");
        LISTENER_TID.store(0, Ordering::SeqCst);
        return;
    };

    if let Err(e) = register_icon(hwnd, hinst) {
        tracing::error!(error = %e, "tray: failed to register notify icon");
        // SAFETY: hwnd is our own message-only window.
        let _ = unsafe { DestroyWindow(hwnd) };
        LISTENER_TID.store(0, Ordering::SeqCst);
        return;
    }
    tracing::info!(tid, "tray icon installed");

    // Standard pump. Tray callbacks arrive as WM_TRAY_CALLBACK and are
    // handled in wnd_proc; nothing to dispatch here that DispatchMessageW
    // doesn't already cover.
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        // SAFETY: msg is populated.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    let _ = unregister_icon(hwnd);
    // SAFETY: hwnd valid.
    let _ = unsafe { DestroyWindow(hwnd) };
    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("tray thread exited");
}

fn register_icon(hwnd: HWND, hinst: HINSTANCE) -> std::result::Result<(), String> {
    // Try our embedded RT_GROUP_ICON first so the tray shows the dwindle
    // logo. Fall back to the OS default `IDI_APPLICATION` if the resource
    // is missing for any reason (e.g. the EXE was stripped) so we always
    // register *some* icon — `Shell_NotifyIconW(NIM_ADD)` will reject a
    // null `hIcon`.
    let custom = PCWSTR(IDI_DWMEND_APP as usize as *const u16);
    let icon = unsafe {
        LoadIconW(Some(hinst), custom)
            .or_else(|_| LoadIconW(Some(hinst), IDI_APPLICATION))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
    }
    .map_err(|e| format!("LoadIconW: {e}"))?;

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    // Tooltip text — up to 127 chars + NUL.
    let tip: Vec<u16> = "DWMend".encode_utf16().collect();
    for (i, &c) in tip.iter().take(127).enumerate() {
        nid.szTip[i] = c;
    }

    // SAFETY: nid is fully initialised with cbSize set; HWND is our own.
    let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    if !ok.as_bool() {
        return Err("Shell_NotifyIconW(NIM_ADD) failed".into());
    }
    Ok(())
}

fn unregister_icon(hwnd: HWND) -> std::result::Result<(), String> {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    // SAFETY: same as NIM_ADD — well-formed struct, our own HWND.
    let ok = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    if !ok.as_bool() {
        return Err("Shell_NotifyIconW(NIM_DELETE) failed".into());
    }
    Ok(())
}

fn create_message_window() -> Option<(HWND, HINSTANCE)> {
    let class_name = utf16(b"DwmendTrayIcon\0");
    // SAFETY: GetModuleHandleW(None) returns the current EXE.
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
    // SAFETY: wnd_class is fully initialised; class_name is null-terminated wstr.
    let atom = unsafe { RegisterClassExW(&wnd_class) };
    if atom == 0 {
        tracing::error!("RegisterClassExW failed for tray");
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
            None,
            Some(hinst),
            None,
        )
    };
    match hwnd {
        Ok(h) => Some((h, hinst)),
        Err(e) => {
            tracing::error!(error = %e, "CreateWindowExW failed for tray");
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
        WM_TRAY_CALLBACK => {
            // The low word of lParam is the mouse message that triggered
            // the callback. We open the menu on right-click and on
            // double-left-click (a common QoL convention).
            let mouse_msg = (lparam.0 as u32) & 0xFFFF;
            if mouse_msg == WM_RBUTTONUP || mouse_msg == WM_LBUTTONDBLCLK {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        // SAFETY: DefWindowProcW is the documented fallback.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn show_menu(hwnd: HWND) {
    // SAFETY: CreatePopupMenu has no preconditions.
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "CreatePopupMenu failed");
            return;
        }
    };

    let pause_label = if PAUSED.load(Ordering::SeqCst) {
        utf16(b"Resume\0")
    } else {
        utf16(b"Pause\0")
    };
    let reload_label = utf16(b"Reload Config\0");
    let cfg_label = utf16(b"Open Config File\0");
    let log_label = utf16(b"Open Log Folder\0");
    let quit_label = utf16(b"Quit\0");

    // SAFETY: menu is a valid HMENU until DestroyMenu; PCWSTR pointers
    // outlive the AppendMenuW calls (we only DestroyMenu at the end).
    unsafe {
        let _ = AppendMenuW(menu, MF_STRING, MENU_ID_PAUSE, PCWSTR(pause_label.as_ptr()));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ID_RELOAD,
            PCWSTR(reload_label.as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ID_OPEN_CONFIG,
            PCWSTR(cfg_label.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_ID_OPEN_LOG_DIR,
            PCWSTR(log_label.as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_ID_QUIT, PCWSTR(quit_label.as_ptr()));
    }

    // MSDN: the owner window must be foreground for TrackPopupMenu to
    // dismiss on an outside click. Failure is non-fatal — the menu still
    // shows; it just won't auto-dismiss until the user clicks an item.
    let _ = unsafe { SetForegroundWindow(hwnd) };

    let mut pt = POINT::default();
    // SAFETY: pt is a valid out-param.
    let _ = unsafe { GetCursorPos(&mut pt) };
    let flags = TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN;

    // SAFETY: menu valid; hwnd valid; flags are documented bits; prcRect=None.
    // With TPM_RETURNCMD the return value IS the menu-item ID (or 0 on
    // cancel / error).
    let id = unsafe { TrackPopupMenu(menu, flags, pt.x, pt.y, Some(0), hwnd, None) };
    // SAFETY: menu came from CreatePopupMenu; DestroyMenu owns it.
    let _ = unsafe { DestroyMenu(menu) };

    // Workaround for the documented "menu doesn't dismiss" issue: post a
    // WM_NULL (msg = 0) so the menu modal loop sees activity and exits.
    let _ = unsafe { PostMessageW(Some(hwnd), 0, WPARAM(0), LPARAM(0)) };

    match id.0 as usize {
        MENU_ID_PAUSE => send_action(TrayAction::TogglePause),
        MENU_ID_RELOAD => send_action(TrayAction::ReloadConfig),
        MENU_ID_OPEN_CONFIG => open_path(CONFIG_PATH.lock().clone()),
        MENU_ID_OPEN_LOG_DIR => open_path(LOG_DIR.lock().clone()),
        MENU_ID_QUIT => send_action(TrayAction::Quit),
        0 => {} // dismissed without selection
        other => tracing::debug!(other, "tray: unknown menu id"),
    }
}

fn send_action(action: TrayAction) {
    if let Some(tx) = EVENT_TX.get() {
        // try_send so a clogged channel can't stall the tray thread; menu
        // actions are user-initiated so dropping one is harmless (they'll
        // click again).
        let _ = tx.try_send(action);
    }
}

fn open_path(p: Option<PathBuf>) {
    let Some(p) = p else { return };
    let wide: Vec<u16> = p
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb = utf16(b"open\0");
    // SAFETY: ShellExecuteW accepts null PCWSTRs for unused fields; both
    // verb and path are valid null-terminated UTF-16 strings.
    let _ = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}
