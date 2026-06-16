//! Newtype identifiers used across the DWMend domain.

use serde::{Deserialize, Serialize};

/// 1-based identifier for a workspace in the global pool.
/// Workspaces are pre-allocated 1..=N at startup; the user never creates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceId(pub u32);

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ws{}", self.0)
    }
}

/// Stable monitor identifier (the platform layer's `MonitorInfo::stable_id`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MonitorId(pub String);

impl std::fmt::Display for MonitorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// HWND-derived window identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowId(pub isize);

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hwnd:{:#x}", self.0)
    }
}

impl From<dwmend_platform::window::Window> for WindowId {
    fn from(w: dwmend_platform::window::Window) -> Self {
        Self(w.0)
    }
}

impl From<WindowId> for dwmend_platform::window::Window {
    fn from(id: WindowId) -> Self {
        Self(id.0)
    }
}
