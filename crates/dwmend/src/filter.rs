//! Manageability filter — decides which windows DWMend controls.
//!
//! Rule of thumb: be permissive at the style level (visible + has a frame +
//! not owned + not cloaked-by-shell) and let the user's `[[rules]]` block
//! in config carve out exceptions for known-bad apps.

use crate::config::{Rule, RuleAction};
use dwmend_platform::window::Window;
use windows::Win32::UI::WindowsAndMessaging::{
    WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Class names we will never manage even if the user enables an override —
/// these are part of the OS shell itself.
pub const SHELL_CLASS_BLOCKLIST: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "NotifyIconOverflowWindow",
    "TaskListThumbnailWnd",
    "TopLevelWindowForOverflowXamlIsland",
    "Windows.UI.Core.CoreWindow",
    "ApplicationFrameHost", // we manage UWP windows by their content HWND, not host
    "MultitaskingViewFrame",
];

/// What `is_manageable` should consider — keeps the function pure & testable.
pub fn is_manageable(win: Window, rules: &[Rule]) -> bool {
    // Liveness + visibility.
    if !win.is_alive() {
        return false;
    }
    if !win.is_visible() {
        return false;
    }

    // Minimized windows live at (-32000, -32000) and have no meaningful
    // monitor affinity. Picking them up at startup leads to the entire
    // minimized set piling onto the focused workspace.
    if win.is_minimized() {
        return false;
    }

    // Cloaked by shell = on another Virtual Desktop = leave alone.
    if win.is_cloaked_by_shell() {
        return false;
    }

    // Owned windows (popups, dialogs owned by an app) are app-controlled.
    if win.owner().is_some() {
        return false;
    }

    let style = win.style();
    let ex_style = win.ex_style();

    // Tool windows are usually palettes / overlays the app expects to manage.
    if (ex_style.0 & WS_EX_TOOLWINDOW.0) != 0 {
        return false;
    }
    // Windows that don't take activation are usually overlays.
    if (ex_style.0 & WS_EX_NOACTIVATE.0) != 0 {
        return false;
    }

    // Heuristic: we want windows that have either a caption (normal app
    // windows) or look like a top-level popup (some apps draw their own).
    let has_caption = (style.0 & WS_CAPTION.0) != 0;
    let is_popup_topish = (style.0 & WS_POPUP.0) != 0;
    if !has_caption && !is_popup_topish {
        return false;
    }

    // Shell blocklist.
    let class = win.class().unwrap_or_default();
    if SHELL_CLASS_BLOCKLIST
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&class))
    {
        return false;
    }

    // User rules — `ignore` rules veto management entirely.
    if let Some(action) = first_matching_action(win, &class, rules)
        && matches!(action, RuleAction::Ignore)
    {
        return false;
    }

    true
}

/// Look up the first matching user rule for this window (used by both
/// `is_manageable` and by `events.rs` for float-on-create rules).
pub fn first_matching_action(win: Window, class: &str, rules: &[Rule]) -> Option<RuleAction> {
    let title = win.title().unwrap_or_default();
    let exe = win.exe_name();
    for rule in rules {
        if rule.matches(&exe, class, &title) {
            return Some(rule.action);
        }
    }
    None
}
