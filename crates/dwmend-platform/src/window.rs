//! `Window` — a thin safe wrapper around an HWND.
//!
//! HWNDs are stored as `isize` so the type is `Send + Sync` and we never have
//! to wrestle with `windows::Win32::Foundation::HWND` (which is itself a
//! transparent isize-wrapper but the borrow checker would treat it as a
//! raw pointer in some contexts).
//!
//! All getters return `Result<T>` so callers can tell "window died mid-call"
//! from "window is in a weird state".

use crate::dwm;
use crate::rect::ToRect;
use crate::{Rect, Result};
use color_eyre::eyre::eyre;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::ProcessStatus::GetModuleFileNameExW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW, GetForegroundWindow,
    GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SW_HIDE, SW_RESTORE,
    SW_SHOWNOACTIVATE, SetForegroundWindow, ShowWindowAsync, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLOSE,
};

/// Identifier-only HWND wrapper. Cheap to copy, `Send + Sync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Window(pub isize);

impl Window {
    #[inline]
    pub fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }

    #[inline]
    pub fn hwnd(self) -> HWND {
        HWND(self.0 as *mut std::ffi::c_void)
    }

    // ---- liveness / state ------------------------------------------------

    pub fn is_alive(self) -> bool {
        // SAFETY: IsWindow is safe with any HWND value.
        unsafe { IsWindow(Some(self.hwnd())).as_bool() }
    }

    pub fn is_visible(self) -> bool {
        // SAFETY: IsWindowVisible is safe with any HWND.
        unsafe { IsWindowVisible(self.hwnd()).as_bool() }
    }

    pub fn is_minimized(self) -> bool {
        unsafe { IsIconic(self.hwnd()).as_bool() }
    }

    pub fn is_maximized(self) -> bool {
        unsafe { IsZoomed(self.hwnd()).as_bool() }
    }

    pub fn style(self) -> WINDOW_STYLE {
        // GetWindowLongPtrW returns 0 on failure; that's an invalid style we
        // treat as "no flags set" which is the safe-conservative interpretation.
        // SAFETY: GWL_STYLE is a valid index for GetWindowLongPtrW.
        let v = unsafe { GetWindowLongPtrW(self.hwnd(), GWL_STYLE) };
        WINDOW_STYLE(v as u32)
    }

    pub fn ex_style(self) -> WINDOW_EX_STYLE {
        let v = unsafe { GetWindowLongPtrW(self.hwnd(), GWL_EXSTYLE) };
        WINDOW_EX_STYLE(v as u32)
    }

    /// The top-most owner-less ancestor. Used for filtering: dialogs/popups
    /// owned by an app usually shouldn't be tiled.
    pub fn owner(self) -> Option<Window> {
        // SAFETY: GetWindow is safe; null return → no owner.
        let owner = unsafe { GetWindow(self.hwnd(), GW_OWNER) };
        match owner {
            Ok(h) if !h.is_invalid() => Some(Self::from_hwnd(h)),
            _ => None,
        }
    }

    /// Root ancestor, walking owner+parent chain.
    pub fn root(self) -> Window {
        // SAFETY: GetAncestor with GA_ROOT is safe.
        let h = unsafe { GetAncestor(self.hwnd(), GA_ROOT) };
        if h.is_invalid() {
            self
        } else {
            Self::from_hwnd(h)
        }
    }

    // ---- identification --------------------------------------------------

    pub fn title(self) -> Result<String> {
        let mut buf = [0u16; 512];
        // SAFETY: buf is a valid slice, len fits in i32.
        let len = unsafe { GetWindowTextW(self.hwnd(), &mut buf) };
        if len <= 0 {
            // Empty title is normal (e.g. for tray helpers) — return empty
            // string rather than an error so callers can pattern-match.
            return Ok(String::new());
        }
        Ok(String::from_utf16_lossy(&buf[..len as usize]))
    }

    pub fn class(self) -> Result<String> {
        let mut buf = [0u16; 256];
        // SAFETY: buf is a valid slice.
        let len = unsafe { GetClassNameW(self.hwnd(), &mut buf) };
        if len <= 0 {
            return Err(eyre!("GetClassNameW returned no data"));
        }
        Ok(String::from_utf16_lossy(&buf[..len as usize]))
    }

    pub fn process_id(self) -> u32 {
        let mut pid: u32 = 0;
        // SAFETY: pid is a valid out-param; thread id is discarded.
        let _tid = unsafe { GetWindowThreadProcessId(self.hwnd(), Some(&mut pid)) };
        pid
    }

    /// Full path of the owning process's executable, or empty on failure.
    /// We never bubble this as an error because many windows belong to system
    /// processes we can't open — that's expected.
    pub fn exe_path(self) -> String {
        let pid = self.process_id();
        if pid == 0 {
            return String::new();
        }
        // SAFETY: OpenProcess returns a HANDLE or error; we Result-check it.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                false,
                pid,
            )
        };
        let Ok(handle) = handle else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        // SAFETY: handle is valid; passing None for hModule means executable path.
        let len = unsafe { GetModuleFileNameExW(Some(handle), None, &mut buf) };
        // The HANDLE is dropped here; OpenProcess returns a CloseHandle-able
        // handle but windows-rs `OwnedHandle` semantics are inconsistent across
        // versions. We rely on the windows-rs `Owned`/`HANDLE` Drop in 0.58+.
        // If the version doesn't auto-close, this leaks one HANDLE per call;
        // since we call this rarely (only on first manage), that's acceptable
        // for v1. Long-term we should wrap with `windows::core::Owned`.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
        if len == 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }

    /// Just the file name (basename) of the executable, e.g. `"Spotify.exe"`.
    pub fn exe_name(self) -> String {
        let path = self.exe_path();
        std::path::Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    // ---- geometry --------------------------------------------------------

    pub fn rect(self) -> Result<Rect> {
        let mut r = RECT::default();
        // SAFETY: r is a valid out-param.
        unsafe {
            GetWindowRect(self.hwnd(), &mut r).map_err(|e| eyre!("GetWindowRect failed: {e}"))?;
        }
        Ok(r.to_rect())
    }

    /// The *visual* rectangle of the window — the bounds users actually see.
    ///
    /// Prefers `DWMWA_EXTENDED_FRAME_BOUNDS` (excludes the invisible
    /// shadow / resize-handle margins DWM adds outside the chrome). Falls
    /// back to `GetWindowRect` if DWM rejects the call (non-composited
    /// windows, very old apps). Use this for any overlay that should hug
    /// the visible frame; use `rect()` when you need the full HWND rect
    /// (e.g. for hit-testing or `SetWindowPos`).
    pub fn visual_rect(self) -> Result<Rect> {
        match dwm::extended_frame_bounds(self.hwnd()) {
            Ok(r) => Ok(r.to_rect()),
            Err(_) => self.rect(),
        }
    }

    // ---- DWM-mediated state ---------------------------------------------

    /// Set the per-window flag that suppresses OS slide/fade animations. Call
    /// once when DWMend first decides to manage the window (and again whenever
    /// the OS resets it, which empirically happens on un-minimise).
    pub fn disable_transitions(self) -> Result<()> {
        dwm::disable_transitions(self.hwnd())
    }

    /// Hide this window so it can be removed from the active workspace.
    ///
    /// **Why `ShowWindowAsync(SW_HIDE)` and not `DwmSetWindowAttribute(DWMWA_CLOAK)`?**
    /// On Windows 11 the DWM cloak SET path now returns `E_ACCESSDENIED
    /// (0x80070005)` for any HWND outside the calling process — confirmed
    /// against Chromium-based apps (VS Code, Vivaldi, Edge). The Get path
    /// (`DWMWA_CLOAKED`) still works cross-process, which is why we keep
    /// using it for the `is_cloaked_by_shell` virtual-desktop check.
    ///
    /// `ShowWindowAsync` posts `WM_SHOWWINDOW` into the target window's
    /// thread queue, which requires no elevation and is honoured by every
    /// well-behaved app. The visible side-effect — the window leaves the
    /// taskbar while hidden — matches user expectations for workspaces
    /// (windows on another workspace shouldn't appear in the taskbar of
    /// the current one).
    pub fn hide(self) -> Result<()> {
        // SAFETY: ShowWindowAsync is safe with any HWND; SW_HIDE is a
        // documented command. The Async variant queues the message on the
        // target thread instead of calling into it synchronously, which is
        // what makes this work cross-process.
        unsafe {
            ShowWindowAsync(self.hwnd(), SW_HIDE)
                .ok()
                .map_err(|e| eyre!("ShowWindowAsync(SW_HIDE) failed: {e}"))
        }
    }

    /// Counterpart to [`hide`](Self::hide): make the window visible again
    /// without stealing focus (uses `SW_SHOWNOACTIVATE`).
    pub fn show(self) -> Result<()> {
        // SAFETY: ShowWindowAsync is safe with any HWND.
        unsafe {
            ShowWindowAsync(self.hwnd(), SW_SHOWNOACTIVATE)
                .ok()
                .map_err(|e| eyre!("ShowWindowAsync(SW_SHOWNOACTIVATE) failed: {e}"))
        }
    }

    pub fn is_cloaked_by_shell(self) -> bool {
        dwm::is_cloaked_by_shell(self.hwnd())
    }

    // ---- focus / restore / close ----------------------------------------

    pub fn restore(self) -> Result<()> {
        // SAFETY: ShowWindowAsync is safe with any HWND.
        unsafe {
            ShowWindowAsync(self.hwnd(), SW_RESTORE)
                .ok()
                .map_err(|e| eyre!("ShowWindowAsync(RESTORE) failed: {e}"))
        }
    }

    /// Bring window to foreground. UAC-elevated foreground windows may reject
    /// this and that is expected — log and continue at the caller.
    pub fn focus(self) -> Result<()> {
        unsafe {
            SetForegroundWindow(self.hwnd())
                .ok()
                .map_err(|e| eyre!("SetForegroundWindow failed: {e}"))
        }
    }

    /// Politely ask the window to close (sends `WM_CLOSE`).
    pub fn close(self) -> Result<()> {
        // SAFETY: PostMessageW is safe for any HWND.
        unsafe {
            PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0))
                .map_err(|e| eyre!("PostMessage(WM_CLOSE) failed: {e}"))
        }
    }
}

/// The current foreground window, or `None` if there is none (e.g. lock screen).
pub fn foreground() -> Option<Window> {
    // SAFETY: GetForegroundWindow takes no args and always returns safely.
    let h = unsafe { GetForegroundWindow() };
    if h.is_invalid() {
        None
    } else {
        Some(Window::from_hwnd(h))
    }
}
