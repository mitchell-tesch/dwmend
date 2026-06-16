//! `WindowManager` — central state and the only thing under the global mutex.
//!
//! All mutating ops live here. Each op:
//! 1. Updates the in-memory state tree.
//! 2. Computes the affected workspaces' target positions.
//! 3. Calls `defer_pos::apply_positions` exactly once per workspace pass.
//!
//! Threading note: the only thread that calls these methods is the main
//! event-loop thread holding the `Mutex<WindowManager>`. All hooks and
//! workers send messages onto channels; the loop drains them serially.
//!
//! The big public surface (focus / move / resize, workspace switching,
//! monitor topology, retile) lives in submodules — each adds its own
//! `impl WindowManager` block:
//! * [`commands`] — intra-workspace commands
//! * [`workspaces`] — multi-workspace operations
//! * [`monitors`] — monitor focus and `reconcile_monitors`
//! * [`retile`] — layout application + shutdown helpers

mod commands;
mod monitors;
mod retile;
mod workspaces;

use crate::ids::{MonitorId, WindowId, WorkspaceId};
use crate::monitor::Monitor;
use crate::window::{ManagedWindow, WindowMode};
use crate::workspace::Workspace;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use dwmend_platform::monitor::MonitorInfo;
use std::collections::{BTreeMap, HashMap};

/// What to do when the user asks to switch to a workspace that is already
/// visible on another monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlreadyVisibleBehaviour {
    /// Move focus to the other monitor.
    FocusOtherMonitor,
    /// Swap the two workspaces between the monitors.
    Swap,
}

/// Outer gaps + inner gap.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gaps {
    pub inner: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

pub struct WindowManager {
    pub monitors: BTreeMap<MonitorId, Monitor>,
    pub workspaces: BTreeMap<WorkspaceId, Workspace>,
    pub windows: HashMap<WindowId, ManagedWindow>,
    pub focused_monitor: Option<MonitorId>,
    pub focused_window: Option<WindowId>,
    pub gaps: Gaps,
    pub on_already_visible: AlreadyVisibleBehaviour,
    pub paused: bool,
}

impl WindowManager {
    pub fn new(
        monitor_infos: Vec<MonitorInfo>,
        workspace_count: u32,
        gaps: Gaps,
        on_already_visible: AlreadyVisibleBehaviour,
    ) -> Result<Self> {
        if monitor_infos.is_empty() {
            return Err(eyre!("no monitors detected — DWMend cannot start"));
        }

        let mut workspaces = BTreeMap::new();
        for i in 1..=workspace_count {
            workspaces.insert(WorkspaceId(i), Workspace::new(WorkspaceId(i)));
        }

        let mut monitors = BTreeMap::new();
        // Assign the first N workspaces 1..=N to monitors in OS order.
        for (i, info) in monitor_infos.into_iter().enumerate() {
            let ws_id = WorkspaceId((i as u32) + 1);
            // If we only have e.g. 3 monitors but 10 workspaces, monitors get
            // ws 1, 2, 3 visible; the rest stay hidden in the pool. If there
            // are MORE monitors than workspaces, extras share by reusing the
            // last available workspace (rare edge case).
            let ws_id = if workspaces.contains_key(&ws_id) {
                ws_id
            } else {
                WorkspaceId(workspace_count.max(1))
            };

            let m = Monitor::new(info, ws_id);
            if let Some(ws) = workspaces.get_mut(&ws_id) {
                ws.active_monitor = Some(m.id.clone());
                ws.last_seen_monitor = Some(m.id.0.clone());
            }
            monitors.insert(m.id.clone(), m);
        }

        let focused = monitors
            .values()
            // Prefer the OS-designated primary so the daemon's notion of
            // "focused" matches what the user is actually looking at.
            .find(|m| m.info.primary)
            .map(|m| m.id.clone())
            .or_else(|| monitors.keys().next().cloned());
        Ok(Self {
            monitors,
            workspaces,
            windows: HashMap::new(),
            focused_monitor: focused,
            focused_window: None,
            gaps,
            on_already_visible,
            paused: false,
        })
    }

    // ---- queries ---------------------------------------------------------

    pub fn focused_workspace_id(&self) -> Option<WorkspaceId> {
        let m = self.monitors.get(self.focused_monitor.as_ref()?)?;
        Some(m.current_workspace)
    }

    pub fn workspace_of_window(&self, w: WindowId) -> Option<WorkspaceId> {
        self.windows.get(&w).map(|mw| mw.workspace)
    }

    pub(super) fn monitor_of_workspace(&self, ws: WorkspaceId) -> Option<MonitorId> {
        self.workspaces.get(&ws)?.active_monitor.clone()
    }

    /// Bind `ws` to `mid` and record the binding on the workspace's
    /// `last_seen_monitor` for monitor-affinity on re-plug. Use this in
    /// place of writing `ws.active_monitor = Some(...)` directly so the
    /// affinity cache never drifts.
    pub(super) fn bind_workspace_to_monitor(&mut self, ws: WorkspaceId, mid: &MonitorId) {
        if let Some(w) = self.workspaces.get_mut(&ws) {
            w.active_monitor = Some(mid.clone());
            w.last_seen_monitor = Some(mid.0.clone());
        }
    }

    /// Update the focused-window highlight. Either side may be `None`
    /// (e.g. at startup, when nothing was focused yet).
    ///
    /// The highlight is the thick rounded overlay frame; we deliberately do
    /// NOT also recolor the OS's 1 px DWM border accent, because the two
    /// visibly stack and look heavy. The overlay uses the configured
    /// `focused_border_color`; the OS draws its default thin frame inside,
    /// matching every other Windows app.
    pub(super) fn apply_focus_borders(&self, new_focus: Option<WindowId>) {
        let prev = self.focused_window;
        if prev == new_focus {
            return;
        }
        // Position the overlay around the new focused window's *visual*
        // bounds (excluding the invisible DWM shadow / resize margins, so
        // the frame is symmetric on all sides).
        match new_focus {
            Some(id) => match dwmend_platform::window::Window(id.0).visual_rect() {
                Ok(r) => dwmend_platform::focus_border::show_around(r),
                Err(_) => dwmend_platform::focus_border::hide(),
            },
            None => {
                dwmend_platform::focus_border::hide();
                // Nothing managed should be focused now — move OS
                // foreground off the previously-focused window (which is
                // usually about to be / already cloaked) so it stops
                // receiving keystrokes. Skipped when there was no previous
                // focus either: no work to do.
                if prev.is_some() {
                    dwmend_platform::focus_sink::take_focus();
                }
            }
        }
    }

    /// Publish a fresh per-monitor snapshot to every status bar. Called
    /// after any state mutation that changes the visible workspace set,
    /// focused window title, or pause state.
    pub fn publish_bar_state(&self) {
        // Pre-compute which workspaces are visible anywhere — bar shows that
        // hint as an outline so users know "ws 3 is open on the other display".
        let visible_anywhere: std::collections::HashSet<WorkspaceId> = self
            .workspaces
            .iter()
            .filter_map(|(id, ws)| ws.is_visible().then_some(*id))
            .collect();
        let has_windows: std::collections::HashSet<WorkspaceId> =
            self.windows.values().map(|mw| mw.workspace).collect();
        let focused_title = self
            .focused_window
            .and_then(|w| self.windows.get(&w))
            .map(|mw| {
                // If this window is in a stack, append a `[pos/total]`
                // indicator so the user can see which member they're on
                // (and that the tile is even a stack at all).
                let stack_marker = self
                    .workspaces
                    .get(&mw.workspace)
                    .and_then(|ws| ws.tree.stack_position(mw.id))
                    .map(|(pos, total)| format!(" [{}/{}]", pos + 1, total))
                    .unwrap_or_default();
                format!("{}{}", mw.title, stack_marker)
            })
            .unwrap_or_default();
        let right_label = self.paused.then(|| "PAUSED".to_string());

        for monitor in self.monitors.values() {
            let mut ws_states: Vec<crate::ui::bar::WorkspaceState> = self
                .workspaces
                .keys()
                .map(|ws_id| crate::ui::bar::WorkspaceState {
                    id: ws_id.0,
                    is_active: monitor.current_workspace == *ws_id,
                    is_visible: visible_anywhere.contains(ws_id),
                    has_windows: has_windows.contains(ws_id),
                })
                .collect();
            ws_states.sort_by_key(|w| w.id);

            crate::ui::bar::update(
                &monitor.id.0,
                crate::ui::bar::BarSnapshot {
                    workspaces: ws_states,
                    focused_title: focused_title.clone(),
                    right_label: right_label.clone(),
                },
            );
        }
    }

    // ---- manage / unmanage ----------------------------------------------

    /// Add a previously-unmanaged window to a workspace.
    ///
    /// Placement rules:
    /// * If the window already lives on a known monitor, it joins the
    ///   workspace currently visible on that monitor. This avoids the
    ///   "every window jumps to one display at startup" bug.
    /// * Otherwise it falls back to the focused workspace.
    pub fn manage(
        &mut self,
        id: WindowId,
        title: String,
        class: String,
        exe_name: String,
    ) -> Result<()> {
        if self.windows.contains_key(&id) {
            return Ok(()); // already managed
        }

        // Decide which workspace this window belongs on.
        let win = dwmend_platform::window::Window(id.0);
        let host_monitor = win
            .rect()
            .ok()
            .and_then(|r| self.monitor_at_point(r.center_x(), r.center_y()));
        // Fallback chain: host monitor → primary monitor → focused.
        // "Primary then focused" matters when the focused monitor is a smaller
        // secondary display — we want orphan windows on the user's main screen,
        // not piling onto whatever the BTreeMap happened to put first.
        let ws_id = host_monitor
            .as_ref()
            .and_then(|mid| self.monitors.get(mid).map(|m| m.current_workspace))
            .or_else(|| {
                self.monitors
                    .values()
                    .find(|m| m.info.primary)
                    .map(|m| m.current_workspace)
            })
            .or_else(|| self.focused_workspace_id())
            .ok_or_else(|| eyre!("no workspace available to host new window"))?;

        tracing::debug!(
            hwnd = %format!("{:#x}", id.0),
            exe = %exe_name,
            class = %class,
            title = %title,
            workspace = ws_id.0,
            host_monitor = ?host_monitor.as_ref().map(|m| &m.0),
            "managing window"
        );

        // Always disable transitions on first encounter — the smoothness lever.
        let _ = win.disable_transitions();

        self.windows.insert(
            id,
            ManagedWindow {
                id,
                workspace: ws_id,
                mode: WindowMode::Tiled,
                title,
                class,
                exe_name,
                hidden_by_us: false,
            },
        );

        let work_area = self.workspace_work_area(ws_id).unwrap_or_default();
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.tree.insert(id, work_area);
        }
        self.apply_focus_borders(Some(id));
        self.focused_window = Some(id);
        self.retile_workspace(ws_id)
    }

    /// Return the MonitorId whose `bounds` contains the given point, or `None`.
    fn monitor_at_point(&self, x: i32, y: i32) -> Option<MonitorId> {
        self.monitors
            .iter()
            .find(|(_, m)| m.info.bounds.contains(x, y))
            .map(|(id, _)| id.clone())
    }

    /// Drop a window from state and re-tile its workspace.
    pub fn unmanage(&mut self, id: WindowId) -> Result<()> {
        let Some(mw) = self.windows.remove(&id) else {
            return Ok(());
        };
        let ws_id = mw.workspace;
        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.tree.remove(id);
            ws.floating.retain(|(w, _)| *w != id);
        }
        if self.focused_window == Some(id) {
            // Move focus to whatever the BSP tree now thinks is focused.
            self.focused_window = self.workspaces.get(&ws_id).and_then(|ws| ws.tree.focused());
        }
        self.retile_workspace(ws_id)
    }

    /// Soft removal: take `id` out of its workspace's BSP tree (and floating
    /// list) and retile, but **keep it in `self.windows`** so a subsequent
    /// Show / Restore / Uncloak event can re-insert it via
    /// [`soft_restore`](Self::soft_restore).
    ///
    /// Used when an app hides or cloaks itself — e.g. tray-minimise from
    /// Spotify, Teams, Slack, Discord. Without this the BSP slot stays
    /// reserved and the workspace shows a blank rectangle where the app
    /// used to be.
    ///
    /// If the removed window held focus, actively transfers it to the
    /// next BSP-focused window on the same workspace, or to the focus
    /// sink if none remain. Apps that tray-minimise via DWM cloak can
    /// keep OS foreground unless we explicitly steal it.
    pub fn soft_remove(&mut self, id: WindowId) -> Result<()> {
        let Some(ws_id) = self.workspace_of_window(id) else {
            return Ok(());
        };

        let (was_tiled, was_floating) = match self.workspaces.get(&ws_id) {
            Some(ws) => (
                ws.tree.contains(&id),
                ws.floating.iter().any(|(w, _)| *w == id),
            ),
            None => return Ok(()),
        };
        if !was_tiled && !was_floating {
            return Ok(()); // already removed; nothing to do
        }

        if let Some(ws) = self.workspaces.get_mut(&ws_id) {
            ws.tree.remove(id);
            ws.floating.retain(|(w, _)| *w != id);
        }

        // If the removed window held focus, hand it off.
        if self.focused_window == Some(id) {
            let next = self.workspaces.get(&ws_id).and_then(|w| w.tree.focused());
            self.apply_focus_borders(next);
            self.focused_window = next;
            if let Some(w) = next {
                if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                    ws.tree.focus(w);
                }
                // Actively foreground — cloaked apps don't always trigger
                // the OS's automatic next-window focus.
                let _ = dwmend_platform::window::Window(w.0).focus();
            }
            // If next is None, apply_focus_borders(None) already hid the
            // overlay and pushed foreground to the focus sink.
        }

        self.retile_workspace(ws_id)
    }

    /// Inverse of [`soft_remove`](Self::soft_remove): re-insert `id` into
    /// its workspace's BSP tree if it's been removed, then retile. Called
    /// on Restored / Uncloaked / re-Shown events for an already-managed
    /// window.
    pub fn soft_restore(&mut self, id: WindowId) -> Result<()> {
        let Some(ws_id) = self.workspace_of_window(id) else {
            return Ok(());
        };

        let already_present = match self.workspaces.get(&ws_id) {
            Some(ws) => ws.tree.contains(&id) || ws.floating.iter().any(|(w, _)| *w == id),
            None => return Ok(()),
        };
        if !already_present {
            let work_area = self.workspace_work_area(ws_id).unwrap_or_default();
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.tree.insert(id, work_area);
            }
        }
        self.retile_workspace(ws_id)
    }

    // ---- focus -----------------------------------------------------------

    pub fn set_foreground(&mut self, id: WindowId) {
        // Safe with any HWND, including our focus-sink window: the early
        // `windows.get(&id)` lookup guards every mutation, and the sink is
        // never inserted into `self.windows`. So an OS-delivered
        // WinEvent::Foreground for the sink simply no-ops here.
        if let Some(mw) = self.windows.get(&id) {
            let ws_id = mw.workspace;
            self.apply_focus_borders(Some(id));
            self.focused_window = Some(id);
            // Make sure the focused monitor matches this workspace's monitor
            // so subsequent commands act on the right one.
            if let Some(mid) = self.monitor_of_workspace(ws_id) {
                self.focused_monitor = Some(mid);
            }
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.tree.focus(id);
            }
        }
    }
}
