//! Intra-workspace commands: focus / move / resize within the BSP tree,
//! monocle and stack toggles, float toggle, and close.

use super::WindowManager;
use crate::window::WindowMode;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use dwmend_layout::bsp::LayoutMode;
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

        // Floating-window branch: translate the stored absolute rect.
        // The next `retile_workspace` clamps the rect into the workspace's
        // work area via `Rect::clamp_inside`, so we don't need to bound
        // here \u2014 the user can hammer the key past the screen edge and
        // the window simply stops at the edge.
        if matches!(
            self.windows.get(&focused).map(|mw| mw.mode),
            Some(WindowMode::Floating)
        ) {
            // 32-px step matches the resize default and komorebi's
            // floating-move keybinding default. Hard-coded because the
            // grammar `move <dir>` doesn't carry a delta; if a user
            // wants finer control they can use `resize` for size +
            // multiple `move` presses for position.
            const STEP: i32 = 32;
            let (dx, dy) = match dir {
                Direction::Left => (-STEP, 0),
                Direction::Right => (STEP, 0),
                Direction::Up => (0, -STEP),
                Direction::Down => (0, STEP),
            };
            let mut moved = false;
            if let Some(ws) = self.workspaces.get_mut(&ws_id)
                && let Some(entry) = ws.floating.iter_mut().find(|(w, _)| *w == focused)
            {
                entry.1.x += dx;
                entry.1.y += dy;
                moved = true;
            }
            tracing::debug!(
                ?dir,
                workspace = ws_id.0,
                hwnd = format!("{:#x}", focused.0),
                moved,
                "move_focused_direction (floating)"
            );
            if moved {
                self.retile_workspace(ws_id)?;
            }
            return Ok(());
        }

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

        // Floating-window branch: change the rect's size + (for left/up)
        // its origin. `resize right +N` grows the right edge; `resize
        // left +N` grows the left edge (origin shifts left, width grows
        // by N). Width / height are clamped to a 64-px minimum so the
        // user can't accidentally shrink the window down to nothing.
        if matches!(
            self.windows.get(&focused).map(|mw| mw.mode),
            Some(WindowMode::Floating)
        ) {
            const MIN_DIM: i32 = 64;
            let mut resized = false;
            if let Some(ws) = self.workspaces.get_mut(&ws_id)
                && let Some(entry) = ws.floating.iter_mut().find(|(w, _)| *w == focused)
            {
                let r = &mut entry.1;
                match dir {
                    Direction::Right => r.w = (r.w + delta_px).max(MIN_DIM),
                    Direction::Down => r.h = (r.h + delta_px).max(MIN_DIM),
                    Direction::Left => {
                        // Grow / shrink the left edge by `delta_px`.
                        // Position moves left when delta_px > 0; if
                        // shrinking past MIN_DIM we cap so the right
                        // edge stays put.
                        let new_w = (r.w + delta_px).max(MIN_DIM);
                        let actual_delta = new_w - r.w;
                        r.x -= actual_delta;
                        r.w = new_w;
                    }
                    Direction::Up => {
                        let new_h = (r.h + delta_px).max(MIN_DIM);
                        let actual_delta = new_h - r.h;
                        r.y -= actual_delta;
                        r.h = new_h;
                    }
                }
                resized = true;
            }
            tracing::debug!(
                ?dir,
                delta_px,
                workspace = ws_id.0,
                hwnd = format!("{:#x}", focused.0),
                resized,
                "resize_focused (floating)"
            );
            if resized {
                self.retile_workspace(ws_id)?;
            }
            return Ok(());
        }

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

    /// Cycle the focused workspace's BSP layout mode (Dwindle \u2194 Spiral).
    ///
    /// Layout mode controls how `BspTree::insert` chooses a split axis;
    /// flipping it leaves existing splits untouched and only affects
    /// future inserts. No retile is required because no rect changed,
    /// but we publish a debug log so users can confirm the toggle fired.
    pub fn toggle_layout_mode(&mut self) -> Result<()> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or_else(|| eyre!("no focused workspace"))?;
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            let new_mode = match ws.tree.layout_mode() {
                LayoutMode::Dwindle => LayoutMode::Spiral,
                LayoutMode::Spiral => LayoutMode::Dwindle,
            };
            ws.tree.set_layout_mode(new_mode);
            tracing::info!(workspace = ws_id.0, mode = ?new_mode, "layout mode toggled");            // Surface the change as a quick toast so the user gets
            // feedback even when the action is invisible (no existing
            // tile reflows) until the next insert.
            crate::ui::toast::show(
                crate::ui::toast::ToastLevel::Info,
                format!(
                    "Layout: {} (ws {})",
                    match new_mode {
                        LayoutMode::Dwindle => "dwindle",
                        LayoutMode::Spiral => "spiral",
                    },
                    ws_id.0
                ),
            );        }
        Ok(())
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
