//! Workspace switching: bring a workspace to the focused monitor, swap
//! two visible workspaces between monitors, and move-and-follow a window
//! to another workspace.

use super::{AlreadyVisibleBehaviour, WindowManager};
use crate::ids::{MonitorId, WindowId, WorkspaceId};
use color_eyre::Result;
use color_eyre::eyre::eyre;
use dwmend_platform::Rect;

impl WindowManager {
    /// Hyprland-style workspace command: bring `target` to the focused
    /// monitor. If `target` is already visible elsewhere, defer to
    /// `on_already_visible` (focus other monitor, or swap).
    pub fn switch_workspace(&mut self, target: WorkspaceId) -> Result<()> {
        if !self.workspaces.contains_key(&target) {
            return Err(eyre!("workspace {} does not exist", target.0));
        }
        let Some(focused_mid) = self.focused_monitor.clone() else {
            return Err(eyre!("no focused monitor"));
        };
        let Some(focused_mon) = self.monitors.get(&focused_mid) else {
            return Err(eyre!("focused monitor not found in state"));
        };
        if focused_mon.current_workspace == target {
            tracing::debug!(target = target.0, "switch_workspace: already current");
            return Ok(()); // no-op
        }

        // Where is `target` right now?
        let target_visible_on = self.monitor_of_workspace(target);
        tracing::debug!(
            target = target.0,
            focused_monitor = %focused_mid,
            target_visible_on = ?target_visible_on.as_ref().map(|m| &m.0),
            behaviour = ?self.on_already_visible,
            "switch_workspace"
        );

        match target_visible_on {
            Some(other_mid) if other_mid != focused_mid => {
                match self.on_already_visible {
                    AlreadyVisibleBehaviour::FocusOtherMonitor => {
                        self.focused_monitor = Some(other_mid);
                        // Focus the workspace's BSP-focused window.
                        let focused = self
                            .workspaces
                            .get(&target)
                            .and_then(|ws| ws.tree.focused());
                        if let Some(w) = focused {
                            self.apply_focus_borders(Some(w));
                            self.focused_window = Some(w);
                            let _ = dwmend_platform::window::Window(w.0).focus();
                        }
                        Ok(())
                    }
                    AlreadyVisibleBehaviour::Swap => {
                        self.swap_workspaces_between_monitors(focused_mid, other_mid, target)
                    }
                }
            }
            _ => {
                // target is hidden — bring it here, send our current to limbo.
                self.swap_workspace_onto_monitor(&focused_mid, target)
            }
        }
    }

    /// Bring `target` to a SPECIFIC monitor rather than the focused one.
    ///
    /// Used by bar-pill clicks: a click on monitor B's bar should bring
    /// the requested workspace to monitor B even if the user's keyboard
    /// focus is currently on monitor A.
    ///
    /// Implemented by temporarily redirecting `focused_monitor` to the
    /// target and delegating to [`switch_workspace`], which already
    /// handles the "already visible elsewhere" / "currently hidden" /
    /// "no-op" cases. The focus shift survives the call so the user's
    /// keyboard focus follows their click \u2014 matching how clicking on a
    /// monitor in Windows itself focuses that display.
    pub fn switch_workspace_on_monitor(
        &mut self,
        target: WorkspaceId,
        clicked_mid: MonitorId,
    ) -> Result<()> {
        if !self.monitors.contains_key(&clicked_mid) {
            return Err(eyre!(
                "switch_workspace_on_monitor: monitor `{}` not present",
                clicked_mid.0
            ));
        }
        self.focused_monitor = Some(clicked_mid);
        self.switch_workspace(target)
    }


    pub(super) fn swap_workspace_onto_monitor(
        &mut self,
        mid: &MonitorId,
        incoming: WorkspaceId,
    ) -> Result<()> {
        // Resolve the workspace currently on this monitor (the "outgoing" one).
        let outgoing = {
            let m = self
                .monitors
                .get(mid)
                .ok_or_else(|| eyre!("monitor missing"))?;
            m.current_workspace
        };

        // Collect outgoing window IDs first so we can drop the workspace borrow
        // before mutating `self.windows`.
        let outgoing_ids: Vec<WindowId> = self
            .workspaces
            .get(&outgoing)
            .map(|ws| ws.all_windows())
            .unwrap_or_default();

        // Hide every window of the outgoing workspace. The platform layer
        // uses ShowWindowAsync(SW_HIDE) because DWM cloaking is denied
        // cross-process on Windows 11; ShowWindow works for all apps and
        // also removes them from the taskbar while hidden — standard
        // workspace semantics.
        for id in &outgoing_ids {
            if let Err(e) = dwmend_platform::window::Window(id.0).hide() {
                tracing::warn!(
                    hwnd = format!("{:#x}", id.0),
                    error = %e,
                    "hide failed on outgoing window"
                );
            }
            if let Some(mw) = self.windows.get_mut(id) {
                mw.hidden_by_us = true;
            }
        }
        if let Some(ws) = self.workspaces.get_mut(&outgoing) {
            ws.active_monitor = None;
        }

        // Bind the incoming workspace to this monitor.
        let incoming_ids: Vec<WindowId> = self
            .workspaces
            .get(&incoming)
            .map(|ws| ws.all_windows())
            .unwrap_or_default();
        self.bind_workspace_to_monitor(incoming, mid);
        for id in &incoming_ids {
            if let Err(e) = dwmend_platform::window::Window(id.0).show() {
                tracing::warn!(
                    hwnd = format!("{:#x}", id.0),
                    error = %e,
                    "show failed on incoming window"
                );
            }
            if let Some(mw) = self.windows.get_mut(id) {
                mw.hidden_by_us = false;
            }
        }

        if let Some(m) = self.monitors.get_mut(mid) {
            m.current_workspace = incoming;
        }

        self.retile_workspace(incoming)?;

        // Focus the incoming workspace's tracked focused window if any.
        let new_focus = self
            .workspaces
            .get(&incoming)
            .and_then(|w| w.tree.focused());
        self.apply_focus_borders(new_focus);
        self.focused_window = new_focus;
        if let Some(w) = new_focus {
            let _ = dwmend_platform::window::Window(w.0).focus();
        }
        Ok(())
    }

    fn swap_workspaces_between_monitors(
        &mut self,
        focused_mid: MonitorId,
        other_mid: MonitorId,
        target: WorkspaceId,
    ) -> Result<()> {
        // The focused monitor's current workspace will trade places with
        // `target` (which lives on `other_mid`).
        let focused_current = self
            .monitors
            .get(&focused_mid)
            .map(|m| m.current_workspace)
            .ok_or_else(|| eyre!("focused monitor missing"))?;

        // Re-bind both workspaces.
        self.bind_workspace_to_monitor(focused_current, &other_mid);
        self.bind_workspace_to_monitor(target, &focused_mid);
        if let Some(m) = self.monitors.get_mut(&focused_mid) {
            m.current_workspace = target;
        }
        if let Some(m) = self.monitors.get_mut(&other_mid) {
            m.current_workspace = focused_current;
        }
        // Retile both — neither needs un-hiding (both were visible already).
        self.retile_workspace(focused_current)?;
        self.retile_workspace(target)?;

        let new_focus = self.workspaces.get(&target).and_then(|w| w.tree.focused());
        self.apply_focus_borders(new_focus);
        self.focused_window = new_focus;
        if let Some(w) = new_focus {
            let _ = dwmend_platform::window::Window(w.0).focus();
        }
        Ok(())
    }

    /// Move the focused window to `target` and **follow the focus**: after
    /// the move the user is on the target workspace with the moved window
    /// focused. If `target` is hidden it is surfaced on the focused monitor
    /// (hiding whatever was there); if `target` is already visible on
    /// another monitor, the focused monitor shifts to that monitor.
    pub fn move_focused_to_workspace(&mut self, target: WorkspaceId) -> Result<()> {
        let Some(id) = self.focused_window else {
            return Ok(());
        };
        let Some(src_ws) = self.windows.get(&id).map(|mw| mw.workspace) else {
            return Ok(());
        };
        if src_ws == target {
            return Ok(());
        }
        if !self.workspaces.contains_key(&target) {
            return Err(eyre!("workspace {} does not exist", target.0));
        }

        // 1. Detach from source.
        if let Some(ws) = self.workspaces.get_mut(&src_ws) {
            ws.tree.remove(id);
            ws.floating.retain(|(w, _)| *w != id);
        }

        // 2. Attach to target. `BspTree::insert` ends with `self.focus =
        //    new_leaf`, so target's BSP cursor now points at our window —
        //    every downstream "focus target.tree.focused()" call picks it up
        //    automatically.
        let target_work_area = self
            .workspace_work_area(target)
            .unwrap_or_else(|| Rect::new(0, 0, 1920, 1080));
        if let Some(ws) = self.workspaces.get_mut(&target) {
            ws.tree.insert(id, target_work_area);
        }
        if let Some(mw) = self.windows.get_mut(&id) {
            mw.workspace = target;
        }

        // 3. Retile source so its remaining tiles reflow into the freed slot.
        self.retile_workspace(src_ws)?;

        // 4. Follow focus to the target workspace.
        let target_visible_on = self.monitor_of_workspace(target);
        match target_visible_on {
            None => {
                // Target hidden → surface it on the focused monitor. This
                // hides whatever was there, shows target's windows (including
                // the one we just moved), retiles target, and focuses its
                // BSP-focused leaf — which is our moved window.
                let focused_mid = self
                    .focused_monitor
                    .clone()
                    .ok_or_else(|| eyre!("no focused monitor"))?;
                self.swap_workspace_onto_monitor(&focused_mid, target)?;
            }
            Some(target_mid) => {
                // Target already visible on another monitor — the moved
                // window may currently still be on the source monitor's
                // screen; make sure it's shown, then retile target so it
                // lands in its new slot on `target_mid`, and shift OS focus.
                let win = dwmend_platform::window::Window(id.0);
                if let Err(e) = win.show() {
                    tracing::warn!(
                        hwnd = format!("{:#x}", id.0),
                        error = %e,
                        "show failed on moved window"
                    );
                }
                if let Some(mw) = self.windows.get_mut(&id) {
                    mw.hidden_by_us = false;
                }
                self.focused_monitor = Some(target_mid);
                self.retile_workspace(target)?;
                self.apply_focus_borders(Some(id));
                self.focused_window = Some(id);
                let _ = win.focus();
            }
        }

        Ok(())
    }
}
