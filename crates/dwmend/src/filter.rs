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

/// Pure-data manageability decision.
///
/// Split out from [`is_manageable`] so the rule logic is unit-testable
/// without a real `HWND`. The wrapper just plumbs Win32-derived values
/// into this function; behaviour is identical for any well-behaved window.
///
/// The audit specifically called this filter out as the
/// "most-dangerous-untested-path" \u2014 a regression that admitted
/// `Shell_TrayWnd` or vetoed every browser would render the WM
/// unusable, and previously there was no way to exercise the decision
/// matrix without launching the daemon.
#[allow(clippy::too_many_arguments)]
pub fn is_manageable_inputs(
    alive: bool,
    visible: bool,
    minimized: bool,
    cloaked_by_shell: bool,
    has_owner: bool,
    style: u32,
    ex_style: u32,
    class: &str,
    title: &str,
    exe: &str,
    rules: &[Rule],
) -> bool {
    if !alive {
        return false;
    }
    if !visible {
        return false;
    }
    // Minimized windows live at (-32000, -32000) and have no meaningful
    // monitor affinity. Picking them up at startup leads to the entire
    // minimized set piling onto the focused workspace.
    if minimized {
        return false;
    }
    // Cloaked by shell = on another Virtual Desktop = leave alone.
    if cloaked_by_shell {
        return false;
    }
    // Owned windows (popups, dialogs owned by an app) are app-controlled.
    if has_owner {
        return false;
    }
    // Tool windows are usually palettes / overlays the app expects to manage.
    if (ex_style & WS_EX_TOOLWINDOW.0) != 0 {
        return false;
    }
    // Windows that don't take activation are usually overlays.
    if (ex_style & WS_EX_NOACTIVATE.0) != 0 {
        return false;
    }
    // Heuristic: we want windows that have either a caption (normal app
    // windows) or look like a top-level popup (some apps draw their own).
    let has_caption = (style & WS_CAPTION.0) != 0;
    let is_popup_topish = (style & WS_POPUP.0) != 0;
    if !has_caption && !is_popup_topish {
        return false;
    }
    // Shell blocklist.
    if SHELL_CLASS_BLOCKLIST
        .iter()
        .any(|c| c.eq_ignore_ascii_case(class))
    {
        return false;
    }
    // User rules \u2014 first match wins; only `ignore` vetoes management.
    for rule in rules {
        if rule.matches(exe, class, title) {
            if matches!(rule.action, RuleAction::Ignore) {
                return false;
            }
            break;
        }
    }
    true
}

/// What `is_manageable` should consider — keeps the function pure & testable.
pub fn is_manageable(win: Window, rules: &[Rule]) -> bool {
    if !win.is_alive() {
        return false;
    }
    let class = win.class().unwrap_or_default();
    let title = win.title().unwrap_or_default();
    let exe = win.exe_name();
    is_manageable_inputs(
        true, // alive
        win.is_visible(),
        win.is_minimized(),
        win.is_cloaked_by_shell(),
        win.owner().is_some(),
        win.style().0,
        win.ex_style().0,
        &class,
        &title,
        &exe,
        rules,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Rule, RuleAction};
    use regex::Regex;
    use windows::Win32::UI::WindowsAndMessaging::{
        WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };

    /// A "normal" window: alive, visible, has caption, not owned, no
    /// blocklist class. The starting point for boundary-condition tests.
    fn normal() -> ManageableArgs {
        ManageableArgs {
            alive: true,
            visible: true,
            minimized: false,
            cloaked_by_shell: false,
            has_owner: false,
            style: WS_CAPTION.0,
            ex_style: 0,
            class: "Mozilla".into(),
            title: "GitHub - Firefox".into(),
            exe: "firefox.exe".into(),
            rules: Vec::new(),
        }
    }

    struct ManageableArgs {
        alive: bool,
        visible: bool,
        minimized: bool,
        cloaked_by_shell: bool,
        has_owner: bool,
        style: u32,
        ex_style: u32,
        class: String,
        title: String,
        exe: String,
        rules: Vec<Rule>,
    }

    impl ManageableArgs {
        fn run(&self) -> bool {
            is_manageable_inputs(
                self.alive,
                self.visible,
                self.minimized,
                self.cloaked_by_shell,
                self.has_owner,
                self.style,
                self.ex_style,
                &self.class,
                &self.title,
                &self.exe,
                &self.rules,
            )
        }
    }

    #[test]
    fn normal_app_window_is_manageable() {
        assert!(normal().run());
    }

    #[test]
    fn dead_window_rejected() {
        let mut a = normal();
        a.alive = false;
        assert!(!a.run());
    }

    #[test]
    fn invisible_window_rejected() {
        let mut a = normal();
        a.visible = false;
        assert!(!a.run());
    }

    #[test]
    fn minimized_window_rejected() {
        let mut a = normal();
        a.minimized = true;
        assert!(!a.run());
    }

    #[test]
    fn cloaked_by_shell_rejected() {
        // Virtual-desktop hidden windows must not be hijacked onto the
        // current desktop \u2014 doing so dumps every off-desktop window onto
        // workspace 1 the moment DWMend starts.
        let mut a = normal();
        a.cloaked_by_shell = true;
        assert!(!a.run());
    }

    #[test]
    fn owned_window_rejected() {
        // Owned popups (Save dialog, color picker) belong to their parent
        // app and would visually fight any tile we placed them in.
        let mut a = normal();
        a.has_owner = true;
        assert!(!a.run());
    }

    #[test]
    fn tool_window_rejected() {
        let mut a = normal();
        a.ex_style |= WS_EX_TOOLWINDOW.0;
        assert!(!a.run());
    }

    #[test]
    fn noactivate_window_rejected() {
        let mut a = normal();
        a.ex_style |= WS_EX_NOACTIVATE.0;
        assert!(!a.run());
    }

    #[test]
    fn no_caption_no_popup_rejected() {
        // Bare child-style window with no caption and no popup bit \u2014
        // typically a borderless overlay / background sink. Shouldn't
        // be tiled.
        let mut a = normal();
        a.style = 0;
        assert!(!a.run());
    }

    #[test]
    fn popup_without_caption_is_manageable() {
        // Some games and Electron apps draw their own chrome and leave
        // WS_CAPTION off but set WS_POPUP. We intentionally accept these
        // so the user's primary launcher / app actually tiles.
        let mut a = normal();
        a.style = WS_POPUP.0;
        assert!(a.run());
    }

    #[test]
    fn shell_class_progman_rejected() {
        // The OS desktop window. Hijacking this would let the user
        // tile their wallpaper \u2014 funny once, breaks Explorer forever.
        let mut a = normal();
        a.class = "Progman".into();
        assert!(!a.run());
    }

    #[test]
    fn shell_class_match_is_case_insensitive() {
        let mut a = normal();
        a.class = "shell_traywnd".into();
        assert!(!a.run());
    }

    #[test]
    fn application_frame_host_rejected() {
        // UWP host: we manage the inner content HWND, not this frame.
        let mut a = normal();
        a.class = "ApplicationFrameHost".into();
        assert!(!a.run());
    }

    #[test]
    fn ignore_rule_vetoes_otherwise_manageable_window() {
        let rule = Rule {
            exe: Some("spotify.exe".into()),
            class_re: None,
            title_re: None,
            action: RuleAction::Ignore,
        };
        let mut a = normal();
        a.exe = "spotify.exe".into();
        a.rules = vec![rule];
        assert!(!a.run());
    }

    #[test]
    fn float_rule_keeps_window_manageable() {
        // Float means "manage but don't tile" \u2014 the WM still owns the
        // window, just lets it float in the BSP tree.
        let rule = Rule {
            exe: Some("calculator.exe".into()),
            class_re: None,
            title_re: None,
            action: RuleAction::Float,
        };
        let mut a = normal();
        a.exe = "calculator.exe".into();
        a.rules = vec![rule];
        assert!(a.run());
    }

    #[test]
    fn first_matching_rule_wins_subsequent_ignored() {
        // Tile-then-Ignore on the same exe: tile wins because it's first.
        // Documents the "first match wins" contract \u2014 if this flips, the
        // implications for user configs are large.
        let r1 = Rule {
            exe: Some("Code.exe".into()),
            class_re: None,
            title_re: None,
            action: RuleAction::Tile,
        };
        let r2 = Rule {
            exe: Some("Code.exe".into()),
            class_re: None,
            title_re: None,
            action: RuleAction::Ignore,
        };
        let mut a = normal();
        a.exe = "Code.exe".into();
        a.rules = vec![r1, r2];
        assert!(a.run());
    }

    #[test]
    fn ignore_rule_with_class_regex_filter() {
        // Only the matching class is vetoed, others stay manageable.
        let rule = Rule {
            exe: None,
            class_re: Some(Regex::new(r"^Chrome_PiP_").unwrap()),
            title_re: None,
            action: RuleAction::Ignore,
        };
        let mut a = normal();
        a.class = "Chrome_PiP_Window".into();
        a.rules = vec![rule.clone()];
        assert!(!a.run());

        let mut b = normal(); // different class
        b.rules = vec![rule];
        assert!(b.run());
    }
}
