//! Background reaper — periodically removes dead HWNDs from state.
//!
//! Some apps die without firing `EVENT_OBJECT_DESTROY` in a way we observe
//! (especially crash-exits or apps that bypass user32). The reaper is the
//! safety net: every N seconds it walks our `windows` map, calls
//! `IsWindow(hwnd)`, and drops anything stale.

use crate::commands::Command;
use crossbeam_channel::Sender;
use std::time::Duration;

/// Spawn a sleep-loop thread that posts `Command::Reap` on `cmd_tx` every
/// `interval`. The reaper is a *safety net* — if we can't spawn it the
/// daemon still functions; dead HWNDs accumulate slowly and are cleaned
/// up by `EVENT_OBJECT_DESTROY` for most apps. Logging the failure beats
/// crashing the whole daemon under OS resource pressure.
pub fn start(cmd_tx: Sender<Command>, interval: Duration) {
    let result = std::thread::Builder::new()
        .name("dwmend-reaper".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                if cmd_tx.send(Command::Reap).is_err() {
                    // main loop is gone; we're done.
                    return;
                }
            }
        });
    if let Err(e) = result {
        tracing::error!(
            error = %e,
            "reaper thread spawn failed; dead HWNDs will rely on \
             EVENT_OBJECT_DESTROY only (most apps fire it)"
        );
    }
}
