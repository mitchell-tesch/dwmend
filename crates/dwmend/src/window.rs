//! A managed window's lightweight record in the global window map.

use crate::ids::{WindowId, WorkspaceId};

/// Per-window state DWMend keeps independent of the BSP tree, including the
/// owning workspace and floating/fullscreen overrides.
///
/// `id`, `class`, and `exe_name` are cached at manage time so future
/// features (rule re-evaluation, debug printing, IPC) can read them without
/// going back to Win32 — which can fail if the window has since died.
#[derive(Debug, Clone)]
pub struct ManagedWindow {
    pub id: WindowId,
    pub workspace: WorkspaceId,
    pub mode: WindowMode,
    pub title: String,
    pub class: String,
    pub exe_name: String,
    /// Set when DWMend has hidden the window via `ShowWindowAsync(SW_HIDE)` so
    /// we know to restore it on shutdown.
    pub hidden_by_us: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowMode {
    /// Participates in the BSP tree.
    Tiled,
    /// Free-floating with explicit position.
    Floating,
}
