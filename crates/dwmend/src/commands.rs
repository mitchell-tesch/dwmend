//! High-level commands the WM accepts from hotkeys / IPC / config reload.
//!
//! `Command` is the wire-level enum; `dispatch` is the only thing
//! `main.rs` calls inside the mutex.

use crate::config::{Rule, RuleAction};
use crate::filter::{first_matching_action, is_manageable};
use crate::ids::{WindowId, WorkspaceId};
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
    StackSwallow(Direction),
    StackPop,
    FocusStackNext,
    FocusStackPrev,
    CloseFocused,

    // ---- workspaces ------------
    SwitchWorkspace(WorkspaceId),
    MoveFocusedToWorkspace(WorkspaceId),

    // ---- monitors --------------
    FocusMonitor(Direction),

    // ---- daemon control --------
    TogglePause,
    ReloadConfig,
    Reap,
    Quit,

    // ---- internal --------------
    /// Sent by the file watcher when the config changes — handled by the host.
    ConfigChanged,
}

/// Dispatch a command. The caller must hold the WM mutex.
pub fn dispatch(wm: &mut WindowManager, cmd: Command) -> Result<()> {
    use Command::*;
    match cmd {
        FocusDirection(d) => wm.focus_direction(d),
        MoveDirection(d) => wm.move_focused_direction(d),
        Resize { dir, delta_px } => wm.resize_focused(dir, delta_px),
        ToggleFloat => wm.toggle_float(),
        ToggleMonocle => wm.toggle_monocle(),
        ToggleStack => wm.toggle_stack(),
        StackSwallow(d) => wm.stack_swallow(d),
        StackPop => wm.stack_pop(),
        FocusStackNext => wm.focus_stack_next(),
        FocusStackPrev => wm.focus_stack_prev(),
        CloseFocused => wm.close_focused(),

        SwitchWorkspace(ws) => wm.switch_workspace(ws),
        MoveFocusedToWorkspace(ws) => wm.move_focused_to_workspace(ws),

        FocusMonitor(d) => wm.focus_monitor_direction(d),

        TogglePause => {
            wm.paused = !wm.paused;
            dwmend_platform::keyboard::PAUSED
                .store(wm.paused, std::sync::atomic::Ordering::Relaxed);
            // Update the tray menu label so the next right-click reads
            // "Resume" instead of "Pause" (and vice versa).
            crate::ui::tray::set_paused(wm.paused);
            tracing::info!(paused = wm.paused, "DWMend pause state changed");
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
    if wm.manage(id, title, class.clone(), exe.clone()).is_err() {
        return false;
    }

    // Apply rule-driven follow-ups.
    if let Some(action) = first_matching_action(win, &class, rules) {
        match action {
            RuleAction::Float => {
                let _ = wm.toggle_float(); // marks as floating + retiles
            }
            RuleAction::Tile => {
                let is_floating = wm
                    .windows
                    .get(&id)
                    .map(|mw| matches!(mw.mode, WindowMode::Floating))
                    .unwrap_or(false);
                if is_floating {
                    let _ = wm.toggle_float();
                }
            }
            RuleAction::Ignore => unreachable!(), // filtered above
        }
    }
    true
}
