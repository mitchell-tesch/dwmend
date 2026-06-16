//! Multi-monitor topology: directional monitor focus and the big
//! `reconcile_monitors` re-plug / unplug machine.

use super::WindowManager;
use crate::ids::{MonitorId, WindowId, WorkspaceId};
use crate::monitor::Monitor;
use color_eyre::Result;
use dwmend_platform::Direction;
use dwmend_platform::monitor::MonitorInfo;
use std::collections::HashMap;

impl WindowManager {
    pub fn focus_monitor_direction(&mut self, dir: Direction) -> Result<()> {
        let Some(cur_mid) = self.focused_monitor.clone() else {
            return Ok(());
        };
        let Some(cur) = self.monitors.get(&cur_mid) else {
            return Ok(());
        };
        let cur_center = (cur.info.bounds.center_x(), cur.info.bounds.center_y());

        let mut best: Option<(MonitorId, i64)> = None;
        for (mid, m) in &self.monitors {
            if mid == &cur_mid {
                continue;
            }
            let (cx, cy) = (m.info.bounds.center_x(), m.info.bounds.center_y());
            let dx = (cx - cur_center.0) as i64;
            let dy = (cy - cur_center.1) as i64;
            let in_dir = match dir {
                Direction::Left => dx < 0,
                Direction::Right => dx > 0,
                Direction::Up => dy < 0,
                Direction::Down => dy > 0,
            };
            if !in_dir {
                continue;
            }
            let dist = dx * dx + dy * dy;
            if best.as_ref().map(|(_, b)| dist < *b).unwrap_or(true) {
                best = Some((mid.clone(), dist));
            }
        }

        if let Some((mid, _)) = best {
            self.focused_monitor = Some(mid.clone());
            // Adopt the focused window of the new monitor's workspace.
            if let Some(m) = self.monitors.get(&mid)
                && let Some(ws) = self.workspaces.get(&m.current_workspace)
                && let Some(w) = ws.tree.focused()
            {
                self.apply_focus_borders(Some(w));
                self.focused_window = Some(w);
                let _ = dwmend_platform::window::Window(w.0).focus();
            }
        }
        Ok(())
    }

    /// Rebuild monitor state from a fresh enumeration.
    ///
    /// ## Behaviour
    ///
    /// **Monitor unplugged.** The vanished monitor is removed from
    /// `self.monitors` and its workspace is *promoted* onto a surviving
    /// monitor so the user's windows stay visible. Whichever workspace
    /// was previously on that monitor goes to the pool, still accessible
    /// via `Alt+N`. Promotion targets the focused monitor first, then any
    /// surviving monitor in OS order; surplus orphans (more dead monitors
    /// than survivors) return to the pool with their windows hidden.
    ///
    /// **Monitor re-plugged.** New monitors prefer the workspace whose
    /// `last_seen_monitor` matches their stable id — i.e. the workspace
    /// that used to live there. This gives the "snap back" effect: unplug
    /// your second monitor, do work, plug it back in, and the windows
    /// return to where they were. If the matched workspace is currently
    /// visible on another (surviving) monitor it's swapped back; the
    /// workspace it displaces becomes the new monitor's previously-shown
    /// workspace or returns to the pool.
    ///
    /// **All monitors gone.** Laptop lid closed with no external display.
    /// All managed windows are hidden; the daemon stays alive and recovers
    /// when a monitor returns.
    ///
    /// **Side-effect:** also calls `bar::sync_monitors` so the per-monitor
    /// status bar HWNDs match the new topology — dead bars are destroyed
    /// (otherwise their stale snapshot lingers as a ghost on the surviving
    /// monitor), new bars are created.
    pub fn reconcile_monitors(&mut self, infos: Vec<MonitorInfo>) -> Result<()> {
        let mut by_id: HashMap<String, MonitorInfo> = infos
            .into_iter()
            .map(|i| (i.stable_id.clone(), i))
            .collect();

        // ---- 1. Detect dead monitors and the workspaces they were showing.
        let gone: Vec<MonitorId> = self
            .monitors
            .keys()
            .filter(|k| !by_id.contains_key(&k.0))
            .cloned()
            .collect();
        let orphaned_workspaces: Vec<WorkspaceId> = gone
            .iter()
            .filter_map(|mid| self.monitors.get(mid).map(|m| m.current_workspace))
            .collect();

        if !gone.is_empty() {
            tracing::info!(
                gone_monitors = gone.len(),
                orphan_workspaces = orphaned_workspaces.len(),
                "monitors removed from topology"
            );
        }

        // ---- 2. Detach dead monitors. We deliberately keep
        //         `last_seen_monitor` set on orphan workspaces so a future
        //         re-plug can route them back. NOTE: we do NOT hide orphan
        //         windows yet — if a surviving monitor can absorb them,
        //         retile is enough; we want to avoid a hide/show flash.
        for mid in &gone {
            self.monitors.remove(mid);
        }
        for ws_id in &orphaned_workspaces {
            if let Some(ws) = self.workspaces.get_mut(ws_id) {
                ws.active_monitor = None;
            }
        }

        // ---- 3. Repair focused_monitor if it died.
        if self
            .focused_monitor
            .as_ref()
            .is_none_or(|mid| !self.monitors.contains_key(mid))
        {
            self.focused_monitor = self
                .monitors
                .values()
                .find(|m| m.info.primary)
                .or_else(|| self.monitors.values().next())
                .map(|m| m.id.clone());
        }

        // ---- 4. Refresh info on surviving monitors (bounds/work_area/dpi).
        let mut to_retile: Vec<WorkspaceId> = Vec::new();
        for (mid, mon) in self.monitors.iter_mut() {
            if let Some(info) = by_id.remove(&mid.0) {
                mon.info = info;
                to_retile.push(mon.current_workspace);
            }
        }

        // ---- 5. Add brand-new monitors. For each, prefer the workspace
        //         whose `last_seen_monitor` matches (snap-back affinity);
        //         fall back to the first hidden workspace.
        let new_infos: Vec<MonitorInfo> = by_id.into_values().collect();
        for info in new_infos {
            let stable_id = info.stable_id.clone();
            let m = Monitor::new(info, WorkspaceId(1));

            // Affinity match: pick the workspace that lived on this monitor
            // before. Prefer one currently hidden (no swap needed); else
            // accept one currently visible on another monitor (we'll swap
            // it back below).
            let affinity_hidden = self
                .workspaces
                .iter()
                .find(|(_, ws)| {
                    !ws.is_visible() && ws.last_seen_monitor.as_deref() == Some(&stable_id)
                })
                .map(|(id, _)| *id);
            let affinity_visible = self
                .workspaces
                .iter()
                .find(|(_, ws)| {
                    ws.is_visible() && ws.last_seen_monitor.as_deref() == Some(&stable_id)
                })
                .map(|(id, _)| *id);

            // Pick: affinity_hidden > affinity_visible > first hidden > ws1.
            let (ws_id, needs_swap_from) = if let Some(id) = affinity_hidden {
                tracing::info!(
                    monitor = %stable_id, workspace = id.0,
                    "monitor returned; restoring its previous workspace from pool"
                );
                (id, None)
            } else if let Some(id) = affinity_visible {
                let current_mid = self.monitor_of_workspace(id);
                tracing::info!(
                    monitor = %stable_id, workspace = id.0,
                    "monitor returned; reclaiming workspace from another monitor"
                );
                (id, current_mid)
            } else {
                let pick = (1..=self.workspaces.len() as u32)
                    .find(|i| {
                        self.workspaces
                            .get(&WorkspaceId(*i))
                            .is_some_and(|ws| !ws.is_visible())
                    })
                    .map(WorkspaceId)
                    .unwrap_or(WorkspaceId(1));
                (pick, None)
            };

            // If we're stealing back a visible workspace, unbind it from
            // its current monitor first. The displaced monitor will then
            // need a replacement workspace; we leave that to the next pass
            // or to subsequent user action (it can `Alt+N` to whatever).
            if let Some(stolen_from) = needs_swap_from
                && let Some(ws) = self.workspaces.get_mut(&ws_id)
            {
                ws.active_monitor = None;
                // Pick a pool workspace for the stolen-from monitor so it
                // doesn't end up showing the same ws as us. Prefer one
                // whose last_seen_monitor matches it; else any hidden.
                let replacement = self
                    .workspaces
                    .iter()
                    .find(|(_, ws)| {
                        !ws.is_visible() && ws.last_seen_monitor.as_deref() == Some(&stolen_from.0)
                    })
                    .map(|(id, _)| *id)
                    .or_else(|| {
                        self.workspaces
                            .iter()
                            .find(|(id, ws)| !ws.is_visible() && **id != ws_id)
                            .map(|(id, _)| *id)
                    });
                if let Some(rep) = replacement {
                    self.bind_workspace_to_monitor(rep, &stolen_from);
                    if let Some(mon) = self.monitors.get_mut(&stolen_from) {
                        mon.current_workspace = rep;
                    }
                    to_retile.push(rep);
                }
            }

            // Bind the chosen workspace to this newly-arrived monitor and
            // unhide its windows.
            let ids: Vec<WindowId> = self
                .workspaces
                .get(&ws_id)
                .map(|ws| ws.all_windows())
                .unwrap_or_default();
            self.bind_workspace_to_monitor(ws_id, &m.id);
            for id in &ids {
                if let Err(e) = dwmend_platform::window::Window(id.0).show() {
                    tracing::warn!(
                        hwnd = format!("{:#x}", id.0),
                        error = %e,
                        "show failed on new-monitor window"
                    );
                }
                if let Some(mw) = self.windows.get_mut(id) {
                    mw.hidden_by_us = false;
                }
            }
            let mut m = m;
            m.current_workspace = ws_id;
            self.monitors.insert(m.id.clone(), m);
            to_retile.push(ws_id);
        }

        if self.focused_monitor.is_none() {
            self.focused_monitor = self.monitors.keys().next().cloned();
        }

        // ---- 6. Re-home any orphans that weren't already adopted by
        //         affinity-matching in step 5. (A workspace gets adopted
        //         when its `last_seen_monitor` matches a returning monitor
        //         — typical unplug+replug.)
        let remaining_orphans: Vec<WorkspaceId> = orphaned_workspaces
            .iter()
            .copied()
            .filter(|ws_id| {
                self.workspaces
                    .get(ws_id)
                    .is_some_and(|ws| !ws.is_visible())
            })
            .collect();

        if !remaining_orphans.is_empty() {
            if self.monitors.is_empty() {
                tracing::warn!("no surviving monitors; hiding all orphan windows");
                for ws_id in &remaining_orphans {
                    let ids = self
                        .workspaces
                        .get(ws_id)
                        .map(|w| w.all_windows())
                        .unwrap_or_default();
                    for id in &ids {
                        let _ = dwmend_platform::window::Window(id.0).hide();
                        if let Some(mw) = self.windows.get_mut(id) {
                            mw.hidden_by_us = true;
                        }
                    }
                }
            } else {
                let mut targets: Vec<MonitorId> = Vec::new();
                if let Some(mid) = self.focused_monitor.clone() {
                    targets.push(mid);
                }
                for mid in self.monitors.keys() {
                    if !targets.contains(mid) {
                        targets.push(mid.clone());
                    }
                }

                let pairs: Vec<(MonitorId, WorkspaceId)> = targets
                    .iter()
                    .cloned()
                    .zip(remaining_orphans.iter().copied())
                    .collect();
                for (target_mid, orphan_ws) in &pairs {
                    tracing::info!(
                        workspace = orphan_ws.0,
                        target_monitor = %target_mid,
                        "promoting orphaned workspace onto surviving monitor"
                    );
                    if let Err(e) = self.swap_workspace_onto_monitor(target_mid, *orphan_ws) {
                        tracing::warn!(
                            workspace = orphan_ws.0,
                            error = %e,
                            "failed to promote orphaned workspace"
                        );
                    }
                }

                for orphan_ws in remaining_orphans.iter().skip(targets.len()) {
                    tracing::info!(
                        workspace = orphan_ws.0,
                        "no monitor available; orphan workspace returned to pool"
                    );
                    let ids = self
                        .workspaces
                        .get(orphan_ws)
                        .map(|w| w.all_windows())
                        .unwrap_or_default();
                    for id in &ids {
                        let _ = dwmend_platform::window::Window(id.0).hide();
                        if let Some(mw) = self.windows.get_mut(id) {
                            mw.hidden_by_us = true;
                        }
                    }
                }
            }
        }

        // ---- 7. Retile surviving + freshly-added workspaces. The
        //         promotion path in step 6 and the swap path in step 5
        //         already retile their targets, but a no-op retile is
        //         cheap and the de-duplication via HashSet is not worth
        //         the extra branching here.
        for ws in to_retile {
            let _ = self.retile_workspace(ws);
        }

        // ---- 8. Sync bar HWNDs to the new monitor set so stale dead-
        //         monitor bars stop drawing their last (now-incorrect)
        //         snapshot on top of the surviving monitor's bar.
        let specs: Vec<crate::ui::bar::BarSpec> = self
            .monitors
            .values()
            .map(|m| crate::ui::bar::BarSpec {
                monitor_id: m.id.0.clone(),
                bounds: m.info.bounds,
            })
            .collect();
        crate::ui::bar::sync_monitors(specs);

        Ok(())
    }
}
