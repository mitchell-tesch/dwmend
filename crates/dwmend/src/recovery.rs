//! Crash-recovery: persist the set of managed HWNDs to disk so that, if DWMend
//! dies hard, the user can run `dwmend restore` and get every hidden window
//! visible again.
//!
//! File: `%LOCALAPPDATA%\dwmend\hwnds.json`
//! Format: `[<isize>, <isize>, ...]`
//!
//! Writes are atomic: write to `.tmp` then rename. The reaper does NOT
//! manage this file \u2014 only `manage` / `unmanage` paths in main do.
//!
//! ## Locking discipline
//!
//! Disk I/O \u2014 including the atomic rename \u2014 can stall arbitrarily on slow
//! storage (OneDrive sync, antivirus scan, network home). The main event
//! loop must therefore NOT hold the `Mutex<WindowManager>` across these
//! writes. `save_ids` takes a borrowed slice prepared by the caller from
//! a short critical section; `save(&WindowManager)` is a convenience for
//! one-shot startup writes where no other thread can possibly be waiting
//! on the lock.
//!
//! ## Periodic snapshot writer thread
//!
//! [`Writer`] dedicates a thread to the periodic snapshot path so a
//! stalled disk never freezes the event loop. The daemon `try_send`s a
//! sorted id-list each tick; the worker drains, coalesces, and writes.
//! The bounded(1) channel intentionally drops on overflow \u2014 a newer
//! snapshot supersedes any pending older one.
use crate::config::data_dir;
use crate::state::WindowManager;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Sender, TrySendError, bounded};
use std::path::PathBuf;
use std::thread::JoinHandle;

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

/// Background writer for periodic recovery snapshots.
///
/// Owns a worker thread that drains a bounded queue of id-lists and
/// writes the most recent one via [`save_ids`]. The producer side
/// (`submit`) is non-blocking: if the queue is full, the new snapshot is
/// dropped on the floor \u2014 a newer one is already inbound, and dropping
/// the oldest is the right policy because each snapshot is a complete
/// replacement of the previous file.
///
/// The daemon owns one of these. Dropping the [`Writer`] (or calling
/// [`Writer::shutdown`]) closes the channel; the worker drains its
/// remaining queue and exits.
pub struct Writer {
    tx: Sender<Vec<isize>>,
    handle: Option<JoinHandle<()>>,
}

impl Writer {
    /// Spawn the worker thread. The worker reads `Vec<isize>` snapshots
    /// from a bounded(1) channel, coalesces consecutive items (always
    /// taking the latest), and writes to `hwnds.json`.
    pub fn start() -> Result<Self> {
        let (tx, rx) = bounded::<Vec<isize>>(1);
        let handle = std::thread::Builder::new()
            .name("dwmend-recovery-writer".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Coalesce: drain anything else queued (cap 1 means
                    // at most one) so we always write the freshest set.
                    let mut latest = first;
                    while let Ok(newer) = rx.try_recv() {
                        latest = newer;
                    }
                    if let Err(e) = save_ids(&latest) {
                        tracing::warn!(error = %e, "recovery snapshot write failed");
                    }
                }
                tracing::debug!("recovery writer thread exited");
            })
            .map_err(|e| eyre!("spawn dwmend-recovery-writer: {e}"))?;
        Ok(Self {
            tx,
            handle: Some(handle),
        })
    }

    /// Submit a snapshot for asynchronous write. Returns `true` if the
    /// snapshot was queued, `false` if the worker is still busy with a
    /// previous write (in which case the caller should retry next tick
    /// rather than treating the write as having happened).
    pub fn submit(&self, ids: Vec<isize>) -> bool {
        match self.tx.try_send(ids) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                // Worker still flushing the previous snapshot \u2014 typical
                // when the disk is briefly slow. The caller's diff check
                // will trip again next tick; nothing to log at warn here.
                tracing::trace!("recovery writer busy; deferring snapshot");
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("recovery writer channel disconnected");
                false
            }
        }
    }

    /// Close the channel and wait for the worker to finish any pending
    /// write. Idempotent \u2014 a no-op if already shut down.
    pub fn shutdown(&mut self) {
        // Replacing `tx` with a closed channel drops the original sender
        // and lets the worker observe the disconnect on its next recv.
        let (closed_tx, closed_rx) = bounded::<Vec<isize>>(0);
        drop(closed_rx);
        let live = std::mem::replace(&mut self.tx, closed_tx);
        drop(live);
        if let Some(h) = self.handle.take() {
            // Joining is bounded by however long the in-flight write
            // takes to complete; on a healthy disk this is microseconds.
            let _ = h.join();
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
