//! Layout application: compute target rects from the BSP tree and apply
//! them in a single `BeginDeferWindowPos` batch. Also the shutdown helper
//! that un-hides every window DWMend hid.

use super::WindowManager;
use crate::ids::{WindowId, WorkspaceId};
use color_eyre::Result;
use dwmend_platform::defer_pos;
use dwmend_platform::{HWND, Rect};

impl WindowManager {
    pub(super) fn workspace_work_area(&self, ws_id: WorkspaceId) -> Option<Rect> {
        let ws = self.workspaces.get(&ws_id)?;
        let mid = ws.active_monitor.as_ref()?;
        let m = self.monitors.get(mid)?;
        Some(m.work_area_with_gap(
            self.gaps.top,
            self.gaps.right,
            self.gaps.bottom,
            self.gaps.left,
        ))
    }

    /// Recompute every position for `ws_id` and apply them in a single
    /// `BeginDeferWindowPos` batch.
    pub fn retile_workspace(&mut self, ws_id: WorkspaceId) -> Result<()> {
        // Only visible workspaces have geometry to apply.
        let Some(work_area) = self.workspace_work_area(ws_id) else {
            tracing::debug!(workspace = ws_id.0, "retile skipped: workspace not visible");
            return Ok(());
        };

        // Compute everything we need while the workspace borrow is alive,
        // then drop it so we can mutate `self.windows` for visibility flips
        // without conflicting borrows.
        struct RetilePlan {
            moves: Vec<(HWND, Rect)>,
            visible_ids: Vec<WindowId>,
            hidden_in_stacks: Vec<WindowId>,
        }

        let plan: RetilePlan = {
            let Some(ws) = self.workspaces.get(&ws_id) else {
                return Ok(());
            };
            if ws.monocle {
                // Monocle mode: every tiled window gets the full area. The
                // focused window's Z-order keeps it visually on top; we do
                // not currently hide non-focused monocle members.
                let all = ws.all_windows();
                let moves: Vec<(HWND, Rect)> = all
                    .iter()
                    .map(|id| (dwmend_platform::window::Window(id.0).hwnd(), work_area))
                    .collect();
                let visible_ids = all;
                RetilePlan {
                    moves,
                    visible_ids,
                    hidden_in_stacks: Vec::new(),
                }
            } else {
                let positions = ws.tree.compute(work_area, self.gaps.inner);
                let mut moves: Vec<(HWND, Rect)> = positions
                    .iter()
                    .map(|(id, r)| (dwmend_platform::window::Window(id.0).hwnd(), *r))
                    .collect();
                let visible_ids: Vec<WindowId> = positions.iter().map(|(id, _)| *id).collect();
                let hidden_in_stacks = ws.tree.hidden_in_stacks();
                // Floating rects are stored as absolute pixel coordinates and
                // can therefore point at the *previous* monitor after a
                // topology change (e.g. unplug + promote-orphan). Clamp each
                // one into the current work area so a re-homed window stays
                // visible instead of stretching off-screen.
                for (id, rect) in &ws.floating {
                    moves.push((
                        dwmend_platform::window::Window(id.0).hwnd(),
                        rect.clamp_inside(work_area),
                    ));
                }
                RetilePlan {
                    moves,
                    visible_ids,
                    hidden_in_stacks,
                }
            }
        };

        // Step 1: un-hide any window that should be visible (e.g. a stack
        // member that just got cycled into focus). Skipping the lookup when
        // the flag is already false keeps this O(visible) most of the time.
        for id in &plan.visible_ids {
            if let Some(mw) = self.windows.get_mut(id)
                && mw.hidden_by_us
            {
                let _ = dwmend_platform::window::Window(id.0).show();
                mw.hidden_by_us = false;
            }
        }

        tracing::debug!(
            workspace = ws_id.0,
            work_area = ?work_area,
            window_count = plan.moves.len(),
            hidden_stack = plan.hidden_in_stacks.len(),
            "retiling workspace"
        );
        for (hwnd, rect) in &plan.moves {
            tracing::trace!(hwnd = %format!("{:#x}", hwnd.0 as isize), ?rect, "target rect");
        }

        // Step 2: batch-apply geometry.
        defer_pos::apply_positions(&plan.moves)?;

        // Step 3: hide non-focused stack members. Mark with `hidden_by_us`
        // so the WinEvent::Hidden listener doesn't interpret our own
        // hide as a tray-minimise from the app.
        for id in &plan.hidden_in_stacks {
            if let Some(mw) = self.windows.get_mut(id)
                && !mw.hidden_by_us
            {
                mw.hidden_by_us = true;
                let _ = dwmend_platform::window::Window(id.0).hide();
            }
        }

        // Refresh the focus overlay so the thick border follows the new
        // geometry. We re-query the *visual* rect via
        // DWMWA_EXTENDED_FRAME_BOUNDS rather than reusing the layout rect
        // we just applied: `SetWindowPos` sizes the full HWND rect, but
        // the visual frame the user sees is inset by the invisible DWM
        // shadow / resize margins (~7 px on three sides).
        if let Some(focused) = self.focused_window
            && let Some(mw) = self.windows.get(&focused)
            && mw.workspace == ws_id
            && let Ok(r) = dwmend_platform::window::Window(focused.0).visual_rect()
        {
            dwmend_platform::focus_border::show_around(r);
        }
        Ok(())
    }

    pub fn retile_all(&mut self) -> Result<()> {
        for ws_id in self.workspaces.keys().copied().collect::<Vec<_>>() {
            let _ = self.retile_workspace(ws_id);
        }
        Ok(())
    }

    /// Apply `mode` to every workspace's BSP tree. Used at startup and on
    /// config reload so the configured layout takes effect everywhere.
    /// Runtime per-workspace toggles via [`Self::toggle_layout_mode`]
    /// will be reset by the next reload \u2014 same semantics i3 / Hyprland
    /// expose for their layout flags.
    pub fn apply_layout_mode_all(&mut self, mode: dwmend_layout::bsp::LayoutMode) {
        for ws in self.workspaces.values_mut() {
            ws.tree.set_layout_mode(mode);
        }
    }

    /// Restore visibility of every window DWMend ever hid. Called from the
    /// ctrl-c / quit path and from `dwmend.exe restore`.
    pub fn restore_all_managed_windows(&mut self) {
        // Hide the focus overlay so it doesn't survive the daemon.
        dwmend_platform::focus_border::hide();
        for (id, mw) in self.windows.iter_mut() {
            if mw.hidden_by_us {
                let _ = dwmend_platform::window::Window(id.0).show();
                mw.hidden_by_us = false;
            }
        }
    }
}
