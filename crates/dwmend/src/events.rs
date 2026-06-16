//! WinEvent → state mutation glue.

use crate::commands::admit;
use crate::config::Rule;
use crate::ids::WindowId;
use crate::state::WindowManager;
use dwmend_platform::winevent::WinEvent;

/// Dispatch a single WinEvent against the WM state.
///
/// Returns `true` if the bar state may have changed (focus, focused title,
/// workspace contents, pause). The caller uses this to skip
/// `publish_bar_state` for noisy events like `LocationChanged` so the bar
/// thread isn't repainted hundreds of times per second during mouse drags
/// or animations. Snapshot equality in `bar::update` is the second line of
/// defense; this is the first.
pub fn handle(wm: &mut WindowManager, rules: &[Rule], event: WinEvent) -> bool {
    if wm.paused {
        // While paused, still track foreground (so re-focus works post-resume)
        // but skip everything structural. Foreground change refreshes the
        // bar's focused-title text, so flag it as bar-dirty.
        if let WinEvent::Foreground(h) = event {
            wm.set_foreground(WindowId(h));
            return true;
        }
        return false;
    }

    match event {
        WinEvent::Shown(h) => {
            let id = WindowId(h);
            let win = dwmend_platform::window::Window(h);
            if wm.windows.contains_key(&id) {
                // Already managed. Skip if it's our own workspace-switch
                // SW_SHOWNOACTIVATE — swap_workspace_onto_monitor drives its
                // own retile. Otherwise the window came back from being
                // hidden by the app itself (tray un-minimise); reinsert into
                // the BSP tree.
                if wm.windows.get(&id).is_some_and(|mw| mw.hidden_by_us) {
                    return false;
                }
                let _ = wm.soft_restore(id);
                return true;
            }
            if admit(wm, rules, win) {
                tracing::debug!(?h, "admitted new window");
            }
            true
        }
        WinEvent::Destroyed(h) => {
            let _ = wm.unmanage(WindowId(h));
            true
        }
        WinEvent::Hidden(h) => {
            // App hid its own window (typically tray-minimise via
            // ShowWindow(SW_HIDE)). Soft-remove from the BSP tree so the
            // slot doesn't sit blank. Skip events caused by our own
            // workspace-switch cloak — those are flagged via hidden_by_us.
            let id = WindowId(h);
            if wm.windows.get(&id).is_some_and(|mw| mw.hidden_by_us) {
                return false;
            }
            if wm.windows.contains_key(&id) {
                let _ = wm.soft_remove(id);
                return true;
            }
            false
        }
        WinEvent::Cloaked(h) => {
            // Another way to tray-minimise on modern Win11 (some apps cloak
            // via DWM rather than calling ShowWindow). Same handling as
            // Hidden: yield the slot + transfer focus.
            let id = WindowId(h);
            if wm.windows.contains_key(&id) {
                let _ = wm.soft_remove(id);
                return true;
            }
            false
        }
        WinEvent::Uncloaked(h) => {
            let id = WindowId(h);
            let win = dwmend_platform::window::Window(h);
            if wm.windows.contains_key(&id) {
                let _ = wm.soft_restore(id);
                return true;
            }
            // OS uncloaked a window we don't know about — it may now be
            // newly manageable (typical pattern after a Virtual Desktop
            // switch back to ours).
            let _ = admit(wm, rules, win);
            true
        }
        WinEvent::Foreground(h) => {
            wm.set_foreground(WindowId(h));
            true
        }
        WinEvent::MoveSizeStart(_) => {
            // Pause tiling for this window — user is dragging. We don't
            // expose drag-to-tile in v1, so just no-op. No bar change.
            false
        }
        WinEvent::MoveSizeEnd(h) => {
            // Window stopped moving. If it's tiled, snap it back to its
            // assigned rect (the BSP tree still owns the layout). Retile
            // doesn't change which workspace/window is focused, so the bar
            // pills and focused title are unchanged — no republish needed.
            let ws_id = wm.windows.get(&WindowId(h)).map(|mw| mw.workspace);
            if let Some(ws_id) = ws_id
                && wm.workspaces.get(&ws_id).is_some_and(|ws| ws.is_visible())
            {
                let _ = wm.retile_workspace(ws_id);
            }
            false
        }
        WinEvent::Minimized(h) => {
            // Title-bar minimise. Same shape as tray-minimise — free the
            // BSP slot, transfer focus to a sibling.
            let id = WindowId(h);
            if wm.windows.contains_key(&id) {
                let _ = wm.soft_remove(id);
                return true;
            }
            false
        }
        WinEvent::Restored(h) => {
            // Window came back from minimise; reinsert into the BSP tree.
            let id = WindowId(h);
            if wm.windows.contains_key(&id) {
                let _ = wm.soft_restore(id);
                return true;
            }
            false
        }
        WinEvent::LocationChanged(_h) => {
            // The platform crate's `winevent` callback now drops
            // `EVENT_OBJECT_LOCATIONCHANGE` before it reaches the channel
            // (see comment there for why). This arm is retained as a
            // defensive no-op so a future feature that re-enables the
            // event source — e.g. drag-to-tile — doesn't accidentally
            // trigger a `wm.lock()` per pixel of mouse motion. Bar
            // state is unchanged.
            false
        }
        WinEvent::NameChanged(h) => {
            // Refresh the cached title so logging and rule re-eval stay
            // accurate. The bar only displays the *focused* window's title,
            // so a non-focused title change does not need a repaint.
            let id = WindowId(h);
            let is_focused = wm.focused_window == Some(id);
            if let Some(mw) = wm.windows.get_mut(&id)
                && let Ok(t) = dwmend_platform::window::Window(h).title()
            {
                mw.title = t;
            }
            is_focused
        }
    }
}
