//! Watcher for `%APPDATA%\dwmend\config.toml`.
//!
//! Many editors (notably Notepad and VS) write through a swap-file, which
//! shows up as a Remove/Create burst instead of a single Write. We coalesce
//! by simply re-reading on any kind of event.

use crate::commands::Command;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::Sender;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::time::{Duration, Instant};

/// Spawn the file watcher. Holds the `Watcher` on the heap for its lifetime
/// (dropping it would unwatch the file). Returns the Watcher so main.rs can
/// keep it alive.
pub fn start(path: &Path, tx: Sender<Command>) -> Result<RecommendedWatcher> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("config path `{}` has no parent directory", path.display()))?;
    let file = path.to_path_buf();

    // notify needs a callback; we coalesce rapid bursts to avoid double-reloads.
    let mut last = Instant::now() - Duration::from_secs(60);
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            // Only react to events on our specific file.
            if !ev.paths.iter().any(|p| p == &file) {
                return;
            }
            match ev.kind {
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                    let now = Instant::now();
                    if now.duration_since(last) < Duration::from_millis(200) {
                        return; // debounce
                    }
                    last = now;
                    let _ = tx.send(Command::ConfigChanged);
                }
                _ => {}
            }
        },
        Config::default(),
    )?;

    // Watch the directory rather than the file so file-replace-via-rename
    // (Notepad / VS) still fires events.
    watcher.watch(parent, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}
