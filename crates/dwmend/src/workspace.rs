//! Workspace — a Hyprland-style addressable container of windows.
//!
//! Workspaces are in a global pool keyed by `WorkspaceId`. At any moment,
//! a workspace is either:
//! * **Visible** on exactly one monitor (`active_monitor = Some(m)`)
//! * **Hidden** in the pool (`active_monitor = None`)
//!
//! When hidden, every window belonging to the workspace is hidden via
//! `dwmend_platform::window::Window::hide` — which uses
//! `ShowWindowAsync(SW_HIDE)` because DWM cloaking is denied cross-process
//! on modern Windows 11. The window also leaves the taskbar while hidden,
//! which matches Hyprland/sway workspace semantics.
//!
//! ## Monitor affinity
//!
//! `last_seen_monitor` remembers the stable id of the monitor a workspace
//! was most recently visible on, even after the workspace returns to the
//! pool. When a monitor is re-plugged, `reconcile_monitors` prefers the
//! workspace whose `last_seen_monitor` matches the returning monitor — so
//! windows snap back to where they were before the unplug.

use crate::ids::{MonitorId, WindowId, WorkspaceId};
use dwmend_layout::bsp::BspTree;
use dwmend_platform::Rect;

#[derive(Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub tree: BspTree<WindowId>,
    /// Floating windows + their explicit positions.
    pub floating: Vec<(WindowId, Rect)>,
    /// `None` => workspace is hidden in the pool.
    pub active_monitor: Option<MonitorId>,
    /// Stable id of the monitor this workspace was most recently visible
    /// on. Persists even when `active_monitor` is `None`, so a re-plug
    /// can route the workspace back to its original monitor.
    pub last_seen_monitor: Option<String>,
    pub monocle: bool,
}

impl Workspace {
    pub fn new(id: WorkspaceId) -> Self {
        Self {
            id,
            tree: BspTree::new(),
            floating: Vec::new(),
            active_monitor: None,
            last_seen_monitor: None,
            monocle: false,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.active_monitor.is_some()
    }

    /// Iterator over every window id in this workspace (tiled + floating).
    /// Includes stack members that are currently hidden behind their stack's
    /// focused member, so workspace-level show/hide passes don't miss them.
    pub fn all_windows(&self) -> Vec<WindowId> {
        let tiled = self.tree.windows();
        let mut v: Vec<WindowId> = Vec::with_capacity(tiled.len() + self.floating.len());
        v.extend(tiled);
        for (id, _) in &self.floating {
            v.push(*id);
        }
        v
    }
}
