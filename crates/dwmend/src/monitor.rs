//! Per-monitor record. Cheap; just enough to map MonitorId → current workspace.

use crate::ids::{MonitorId, WorkspaceId};
use dwmend_platform::Rect;
use dwmend_platform::monitor::MonitorInfo;

#[derive(Debug, Clone)]
pub struct Monitor {
    pub id: MonitorId,
    pub info: MonitorInfo,
    /// The workspace currently visible on this monitor.
    pub current_workspace: WorkspaceId,
}

impl Monitor {
    pub fn new(info: MonitorInfo, current_workspace: WorkspaceId) -> Self {
        let id = MonitorId(info.stable_id.clone());
        Self {
            id,
            info,
            current_workspace,
        }
    }

    /// Work area minus the user-configured outer gap.
    pub fn work_area_with_gap(
        &self,
        outer_top: i32,
        outer_right: i32,
        outer_bottom: i32,
        outer_left: i32,
    ) -> Rect {
        self.info
            .work_area
            .inset_each(outer_left, outer_top, outer_right, outer_bottom)
    }
}
