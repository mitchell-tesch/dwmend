//! High-level commands the WM accepts from hotkeys / IPC / config reload.
//!
//! `Command` is the wire-level enum; `dispatch` is the only thing
//! `main.rs` calls inside the mutex.

use crate::config::{Rule, RuleAction};
use crate::filter::{first_matching_action, is_manageable};
use crate::ids::{MonitorId, WindowId, WorkspaceId};
use crate::state::WindowManager;
use crate::window::WindowMode;
use color_eyre::Result;
use dwmend_platform::Direction;
#[derive(Debug, Clone)]
pub enum Command {
    // ---- intra-workspace -------
    FocusDirection(Direction),
    MoveDirection(Direction),
    Resize {
        dir: Direction,
        delta_px: i32,
    },
    ToggleFloat,
    ToggleMonocle,
    ToggleStack,
    /// Cycle the focused workspace through layout modes
    /// (Dwindle \u2192 Spiral \u2192 Dwindle\u2026). Affects future inserts on the
    /// workspace; existing splits keep their axes.
    ToggleLayoutMode,
    StackSwallow(Direction),
    StackPop,
    FocusStackNext,
    FocusStackPrev,
    CloseFocused,

    // ---- workspaces ------------
    SwitchWorkspace(WorkspaceId),
    /// Switch the workspace shown on a SPECIFIC monitor (rather than the
    /// focused one). Used by bar-pill clicks: clicking pill 3 on
    /// monitor B should bring workspace 3 to monitor B even when the
    /// user's focus is currently on monitor A.
    SwitchWorkspaceOnMonitor(WorkspaceId, MonitorId),
    MoveFocusedToWorkspace(WorkspaceId),

    // ---- monitors --------------
    FocusMonitor(Direction),

    // ---- daemon control --------
    TogglePause,
    ReloadConfig,
    /// Show a transient on-screen notification. Used by the IPC
    /// `notify <level> <text>` action. Levels: `info` / `warn` /
    /// `error`. The toast surfaces on the focused monitor; ignored
    /// silently when the toast subsystem is disabled or hasn't
    /// started.
    Notify(crate::ui::toast::ToastLevel, String),

    // ---- window peek -----------
    /// Open the workspace picker overlay (sticky mode), or close
    /// it if it's already open. While the overlay is up, the
    /// existing `focus left/right/up/down` bindings cycle the
    /// highlight instead of moving WM focus.
    PeekToggle,
    /// Commit the current peek selection: focus the highlighted
    /// window and dismiss. No-op when peek isn't open.
    PeekConfirm,

    Reap,
    Quit,

    // ---- internal --------------
    /// Sent by the file watcher when the config changes — handled by the host.
    ConfigChanged,
}

/// Dispatch a command. The caller must hold the WM mutex.
pub fn dispatch(wm: &mut WindowManager, cmd: Command) -> Result<()> {
    use Command::*;

    // Auto-dismiss peek for any non-peek, non-focus-direction command
    // that arrives while the picker is open. Peek is a momentary
    // modal — pressing any other action implicitly closes it before
    // the action runs (so `Alt+1` while peeking still switches
    // workspace, the picker just gets out of the way first).
    //
    // FocusDirection is intercepted INSIDE `wm.focus_direction` to
    // cycle the highlight, so we don't dismiss for it here.
    let preserves_peek = matches!(cmd, FocusDirection(_) | PeekToggle | PeekConfirm);
    if !preserves_peek && crate::ui::peek::is_open() {
        crate::ui::peek::dismiss();
    }

    match cmd {
        FocusDirection(d) => wm.focus_direction(d),
        MoveDirection(d) => wm.move_focused_direction(d),
        Resize { dir, delta_px } => wm.resize_focused(dir, delta_px),
        ToggleFloat => wm.toggle_float(),
        ToggleMonocle => wm.toggle_monocle(),
        ToggleStack => wm.toggle_stack(),
        ToggleLayoutMode => wm.toggle_layout_mode(),
        StackSwallow(d) => wm.stack_swallow(d),
        StackPop => wm.stack_pop(),
        FocusStackNext => wm.focus_stack_next(),
        FocusStackPrev => wm.focus_stack_prev(),
        CloseFocused => wm.close_focused(),

        SwitchWorkspace(ws) => wm.switch_workspace(ws),
        SwitchWorkspaceOnMonitor(ws, mid) => wm.switch_workspace_on_monitor(ws, mid),
        MoveFocusedToWorkspace(ws) => wm.move_focused_to_workspace(ws),

        FocusMonitor(d) => wm.focus_monitor_direction(d),

        PeekToggle => wm.peek_toggle(),
        PeekConfirm => wm.peek_confirm(),

        TogglePause => {
            wm.paused = !wm.paused;
            dwmend_platform::keyboard::PAUSED
                .store(wm.paused, std::sync::atomic::Ordering::Relaxed);
            // Update the tray menu label so the next right-click reads
            // "Resume" instead of "Pause" (and vice versa).
            crate::ui::tray::set_paused(wm.paused);
            tracing::info!(paused = wm.paused, "DWMend pause state changed");
            crate::ui::toast::show(
                crate::ui::toast::ToastLevel::Info,
                if wm.paused {
                    "Paused".to_string()
                } else {
                    "Resumed".to_string()
                },
            );
            Ok(())
        }
        Notify(level, text) => {
            // Routed via the command channel so external IPC clients
            // and bar-pill clicks share one entry point. The toast
            // subsystem itself decides the target monitor (the most
            // recently focused one published via `set_default_monitor`).
            crate::ui::toast::show(level, text);
            Ok(())
        }
        // ReloadConfig / Quit / Reap / ConfigChanged are handled in main.rs
        // because they need access to non-WM state (config file path,
        // hotkey table, listener handles).
        ReloadConfig | Quit | Reap | ConfigChanged => Ok(()),
    }
}

/// Apply the user's rules to a newly-created window. Returns true if it
/// should be managed (False = explicitly ignored).
pub fn admit(wm: &mut WindowManager, rules: &[Rule], win: dwmend_platform::window::Window) -> bool {
    if !is_manageable(win, rules) {
        return false;
    }
    let class = win.class().unwrap_or_default();
    let title = win.title().unwrap_or_default();
    let exe = win.exe_name();

    let id = WindowId(win.0);
    let action = first_matching_action(win, &class, rules);

    // Workspace routing happens INSIDE `manage_routed` so the new window
    // is placed on the target workspace from the very first retile,
    // avoiding a brief flash on the focused workspace before being moved.
    // We still fall back to the default admit when the configured target
    // workspace doesn't exist (typo / out-of-range N) so the rule failure
    // doesn't drop the window.
    let routed = match action {
        Some(RuleAction::Workspace(n)) => {
            let target = WorkspaceId(n);
            if wm.workspaces.contains_key(&target) {
                Some(target)
            } else {
                tracing::warn!(
                    workspace = n,
                    exe = %exe,
                    class = %class,
                    "rule routes to nonexistent workspace; falling back to default placement"
                );
                None
            }
        }
        _ => None,
    };

    let manage_result = match routed {
        Some(target) => wm.manage_routed(id, title, class.clone(), exe.clone(), target),
        None => wm.manage(id, title, class.clone(), exe.clone()),
    };
    if manage_result.is_err() {
        return false;
    }

    // Apply rule-driven mode follow-ups. `Workspace` is a placement-only
    // action; the window stays in its default Tiled mode. `Ignore` is
    // already filtered out by `is_manageable`.
    if let Some(action) = action {
        match action {
            RuleAction::Float => {
                // toggle_float operates on the focused window. For routed
                // windows that didn't take focus, we'd need a target-id
                // version; for v1 we only honour `float` when the window
                // is on the focused workspace (the common case). Skip
                // silently otherwise so the rule doesn't float something
                // on a different workspace than the user is looking at.
                if wm.focused_window == Some(id) {
                    let _ = wm.toggle_float();
                }
            }
            RuleAction::Tile => {
                let is_floating = wm
                    .windows
                    .get(&id)
                    .map(|mw| matches!(mw.mode, WindowMode::Floating))
                    .unwrap_or(false);
                if is_floating && wm.focused_window == Some(id) {
                    let _ = wm.toggle_float();
                }
            }
            RuleAction::Workspace(_) => {
                // Already handled above — placement only.
            }
            RuleAction::Ignore => unreachable!(), // filtered above
        }
    }
    true
}
