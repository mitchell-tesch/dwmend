//! Intra-workspace commands: focus / move / resize within the BSP tree,
//! monocle and stack toggles, float toggle, and close.

use super::WindowManager;
use crate::window::WindowMode;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use dwmend_platform::{Direction, Rect};

impl WindowManager {
    pub fn focus_direction(&mut self, dir: Direction) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        let work_area = self.workspace_work_area(ws_id).unwrap_or_default();
        let current_focus = self.focused_window;
        let next = self
            .workspaces
            .get_mut(&ws_id)
            .and_then(|ws| ws.tree.focus_in_direction(dir, work_area));
        tracing::debug!(
            ?dir,
            workspace = ws_id.0,
            current = ?current_focus.map(|w| format!("{:#x}", w.0)),
            next = ?next.map(|w| format!("{:#x}", w.0)),
            "focus_direction"
        );
        if let Some(t) = next {
            self.apply_focus_borders(Some(t));
            self.focused_window = Some(t);
            match dwmend_platform::window::Window(t.0).focus() {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(error = %e, hwnd = format!("{:#x}", t.0), "SetForegroundWindow failed")
                }
            }
        }
        Ok(())
    }

    // ---- structural commands inside a workspace -------------------------

    pub fn move_focused_direction(&mut self, dir: Direction) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        let Some(focused) = self.focused_window else {
            tracing::debug!(?dir, "move_focused_direction: no focused window");
            return Ok(());
        };
        let work_area = self.workspace_work_area(ws_id).unwrap_or_default();
        let moved = self
            .workspaces
            .get_mut(&ws_id)
            .map(|ws| ws.tree.move_in_direction(focused, dir, work_area))
            .unwrap_or(false);
        tracing::debug!(
            ?dir,
            workspace = ws_id.0,
            hwnd = format!("{:#x}", focused.0),
            moved,
            "move_focused_direction"
        );
        if moved {
            self.retile_workspace(ws_id)?;
        }
        Ok(())
    }

    pub fn resize_focused(&mut self, dir: Direction, delta_px: i32) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        let Some(focused) = self.focused_window else {
            return Ok(());
        };
        let work_area = self.workspace_work_area(ws_id).unwrap_or_default();
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.tree.resize(focused, dir, delta_px, work_area);
        }
        self.retile_workspace(ws_id)
    }

    pub fn toggle_monocle(&mut self) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.monocle = !ws.monocle;
        }
        self.retile_workspace(ws_id)
    }

    /// Toggle stack mode on the focused tile. Smart by design: when the
    /// focused leaf has a Leaf or Stack as its immediate sibling under a
    /// Split, the two are merged into a single Stack so a single
    /// keypress produces a visible change. Only when the sibling is a
    /// subtree (or there is no sibling) does the toggle fall back to
    /// converting the focused leaf alone into a 1-member stack.
    ///
    /// Toggling a multi-member stack expands it back into a chain of
    /// vertical splits.
    pub fn toggle_stack(&mut self) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        let changed = self
            .workspaces
            .get_mut(&ws_id)
            .map(|ws| ws.tree.toggle_stack_focused())
            .unwrap_or(false);
        // Logged at info! (not debug!) so the user can confirm the
        // keybinding fires by tailing `%LOCALAPPDATA%\dwmend\dwmend.log.*`.
        tracing::info!(
            workspace = ws_id.0,
            changed,
            stack = ?self
                .workspaces
                .get(&ws_id)
                .and_then(|ws| ws.tree.focused_stack_info()),
            "toggle_stack"
        );
        if changed {
            // Toggling a multi-member stack off creates new Leaf nodes; the
            // currently-focused window id may have moved to a different
            // node. Re-resolve from the tree's focus pointer so the host's
            // `focused_window` stays in sync.
            let new_focus = self.workspaces.get(&ws_id).and_then(|ws| ws.tree.focused());
            if new_focus.is_some() {
                self.focused_window = new_focus;
            }
        }
        self.retile_workspace(ws_id)
    }

    /// Pull the neighbour in `dir` into the focused tile, forming or
    /// extending a Stack. Visible effect: the neighbour disappears (it's
    /// now hidden behind the focused stack member) and the focused tile
    /// expands to cover the neighbour's former area too.
    pub fn stack_swallow(&mut self, dir: Direction) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        let work_area = self.workspace_work_area(ws_id).unwrap_or_default();
        let swallowed = self
            .workspaces
            .get_mut(&ws_id)
            .map(|ws| ws.tree.stack_swallow_dir(dir, work_area))
            .unwrap_or(false);
        tracing::info!(workspace = ws_id.0, ?dir, swallowed, "stack_swallow");
        if swallowed {
            // The swallowed window becomes the focused stack member.
            let new_focus = self.workspaces.get(&ws_id).and_then(|ws| ws.tree.focused());
            if let Some(w) = new_focus {
                self.focused_window = Some(w);
                let _ = dwmend_platform::window::Window(w.0).focus();
            }
            self.retile_workspace(ws_id)?;
        }
        Ok(())
    }

    /// Pop the focused stack member out of its stack and back into a
    /// standalone tile via a fresh vertical split. Inverse of
    /// `stack_swallow`. No-op when the focused tile isn't a stack with
    /// more than one member.
    pub fn stack_pop(&mut self) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        let popped = self
            .workspaces
            .get_mut(&ws_id)
            .map(|ws| ws.tree.stack_pop_focused())
            .unwrap_or(false);
        tracing::info!(workspace = ws_id.0, popped, "stack_pop");
        if popped {
            let new_focus = self.workspaces.get(&ws_id).and_then(|ws| ws.tree.focused());
            if let Some(w) = new_focus {
                self.focused_window = Some(w);
                let _ = dwmend_platform::window::Window(w.0).focus();
            }
            self.retile_workspace(ws_id)?;
        }
        Ok(())
    }

    /// Cycle focus forward within the focused stack, if any.
    pub fn focus_stack_next(&mut self) -> Result<()> {
        self.cycle_stack(true)
    }

    /// Cycle focus backward within the focused stack, if any.
    pub fn focus_stack_prev(&mut self) -> Result<()> {
        self.cycle_stack(false)
    }

    fn cycle_stack(&mut self, forward: bool) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        let new_focus = self.workspaces.get_mut(&ws_id).and_then(|ws| {
            if forward {
                ws.tree.focus_stack_next()
            } else {
                ws.tree.focus_stack_prev()
            }
        });
        if let Some(w) = new_focus {
            self.focused_window = Some(w);
            // retile_workspace will un-hide the newly-focused stack member
            // and hide whichever member was visible before.
            self.retile_workspace(ws_id)?;
            // Bring OS focus to the new window so keyboard input goes to it.
            let _ = dwmend_platform::window::Window(w.0).focus();
        }
        Ok(())
    }

    pub fn toggle_float(&mut self) -> Result<()> {
        let Some(id) = self.focused_window else {
            return Ok(());
        };
        let Some(mw) = self.windows.get_mut(&id) else {
            return Ok(());
        };
        let ws_id = mw.workspace;
        match mw.mode {
            WindowMode::Tiled => {
                mw.mode = WindowMode::Floating;
                let rect = dwmend_platform::window::Window(id.0)
                    .rect()
                    .unwrap_or(Rect::new(100, 100, 800, 600));
                if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                    ws.tree.remove(id);
                    ws.floating.push((id, rect));
                }
            }
            WindowMode::Floating => {
                mw.mode = WindowMode::Tiled;
                let work_area = self.workspace_work_area(ws_id).unwrap_or_default();
                if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                    ws.floating.retain(|(w, _)| *w != id);
                    ws.tree.insert(id, work_area);
                }
            }
        }
        self.retile_workspace(ws_id)
    }

    pub fn close_focused(&self) -> Result<()> {
        let Some(id) = self.focused_window else {
            return Ok(());
        };
        dwmend_platform::window::Window(id.0).close()
    }
}
