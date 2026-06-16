//! Crash-recovery: persist the set of managed HWNDs to disk so that, if DWMend
//! dies hard, the user can run `dwmend restore` and get every hidden window
//! visible again.
//!
//! File: `%LOCALAPPDATA%\dwmend\hwnds.json`
//! Format: `[<isize>, <isize>, ...]`
//!
//! Writes are atomic: write to `.tmp` then rename. The reaper does NOT
//! manage this file — only `manage` / `unmanage` paths in main do.
//!
//! ## Locking discipline
//!
//! Disk I/O — including the atomic rename — can stall arbitrarily on slow
//! storage (OneDrive sync, antivirus scan, network home). The main event
//! loop must therefore NOT hold the `Mutex<WindowManager>` across these
//! writes. `save_ids` takes a borrowed slice prepared by the caller from
//! a short critical section; `save(&WindowManager)` is a convenience for
//! one-shot startup writes where no other thread can possibly be waiting
//! on the lock.
use crate::config::data_dir;
use crate::state::WindowManager;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use std::path::PathBuf;

pub fn snapshot_path() -> Result<PathBuf> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("hwnds.json"))
}

/// Atomically write `ids` to the snapshot file. Caller is expected to
/// already have copied the id list out of `WindowManager` so this function
/// can be invoked WITHOUT holding the WM mutex — every operation here is
/// disk I/O.
pub fn save_ids(ids: &[isize]) -> Result<()> {
    let path = snapshot_path()?;
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(ids).map_err(|e| eyre!("serialize hwnds: {e}"))?;
    std::fs::write(&tmp, bytes).map_err(|e| eyre!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| eyre!("rename {}: {e}", path.display()))?;
    Ok(())
}

/// Convenience wrapper over [`save_ids`] for callers that already hold a
/// `WindowManager` reference (initial setup pass). Do NOT call this from
/// the event loop while holding `wm.lock()` — copy the ids first and use
/// `save_ids` instead.
pub fn save(wm: &WindowManager) -> Result<()> {
    let ids: Vec<isize> = wm.windows.keys().map(|w| w.0).collect();
    save_ids(&ids)
}

/// Remove the snapshot — called on clean shutdown so a subsequent `restore`
/// is a no-op rather than touching live windows from a previous session.
pub fn clear() -> Result<()> {
    let path = snapshot_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(eyre!("remove {}: {e}", path.display())),
    }
}

/// The `dwmend restore` subcommand: read the snapshot file, then for each HWND
/// uncloak + restore. Failures are non-fatal (some HWNDs will be dead).
pub fn run_restore() -> Result<()> {
    let path = snapshot_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("no snapshot to restore at {}", path.display());
            return Ok(());
        }
        Err(e) => return Err(eyre!("read {}: {e}", path.display())),
    };
    let ids: Vec<isize> = serde_json::from_slice(&bytes)
        .map_err(|e| eyre!("parse snapshot {}: {e}", path.display()))?;

    let mut ok = 0usize;
    let mut dead = 0usize;
    for id in ids {
        let w = dwmend_platform::window::Window(id);
        if !w.is_alive() {
            dead += 1;
            continue;
        }
        // `show()` is `ShowWindowAsync(SW_SHOWNOACTIVATE)` — brings the
        // window back without stealing focus. `restore` un-minimises any
        // window that was minimised when DWMend died.
        let _ = w.show();
        let _ = w.restore();
        ok += 1;
    }
    tracing::info!(restored = ok, dead, "restore complete");
    // Whether successful or not, drop the snapshot — the next normal run
    // will rebuild it from scratch.
    let _ = std::fs::remove_file(&path);
    Ok(())
}
