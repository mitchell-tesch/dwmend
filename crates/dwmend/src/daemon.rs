//! The daemon entry points: `run_daemon` (the real WM event loop) and
//! `run_dryrun` (the read-only diagnostic).
//!
//! Bootstrap order for `run_daemon`:
//!  1. Single-instance check
//!  2. Panic hook
//!  3. PerMonitorV2 DPI awareness
//!  4. AllowSetForegroundWindow (retries 5×)
//!  5. Load config + emit default if missing
//!  6. Spawn WinEvent listener thread
//!  7. Spawn display-change listener thread
//!  8. Build hotkey table, spawn keyboard hook thread
//!  9. Spawn config watcher
//! 10. Build initial WindowManager from EnumWindows
//! 11. Apply initial layout
//! 12. Install Ctrl-C → Quit
//! 13. Enter `select!` loop
//! 14. On exit: restore visibility of every managed window + clear snapshot

use crate::commands::{Command, admit, dispatch};
use crate::state::{Gaps, WindowManager};
use crate::{config, events, filter, hotkey, ipc, reaper, recovery, runtime, ui, watcher};
use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Receiver, Sender, select, unbounded};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

/// Diagnostic: print every detected monitor and every window that would (or
/// wouldn't) be managed, with reasons. Touches nothing on the desktop.
pub fn run_dryrun() -> Result<()> {
    dwmend_platform::dpi::set_per_monitor_v2()?;

    let monitors = dwmend_platform::monitor::enumerate()?;
    println!("\n=== MONITORS ({}) ===", monitors.len());
    for (i, m) in monitors.iter().enumerate() {
        let primary = if m.primary { " [PRIMARY]" } else { "" };
        println!(
            "[{i}] {} dpi={}{}\n    bounds    = {:?}\n    work_area = {:?}\n    stable_id = {}",
            m.friendly_name, m.dpi, primary, m.bounds, m.work_area, m.stable_id
        );
    }

    // Use the user's actual config for rule matching.
    let config_path = config::default_path()?;
    let _ = config::ensure_default(&config_path);
    let cfg = config::Config::load(&config_path)?;
    let rules = cfg.compile_rules()?;

    let all = dwmend_platform::enumerate::enumerate_top_level()?;
    let mut managed = 0usize;
    let mut skipped = 0usize;

    println!("\n=== WINDOWS ===");
    for win in &all {
        let title = win.title().unwrap_or_default();
        let class = win.class().unwrap_or_default();
        let exe = win.exe_name();
        let visible = win.is_visible();
        let cloaked_shell = win.is_cloaked_by_shell();
        let owner = win.owner().is_some();
        let alive = win.is_alive();
        let manage = filter::is_manageable(*win, &rules);

        if !alive || !visible {
            continue; // dead/hidden — not interesting for the report
        }

        let tag = if manage { "MANAGE " } else { "SKIP   " };
        if manage {
            managed += 1;
        } else {
            skipped += 1;
        }
        println!(
            "{tag} hwnd={:#x} class={:?} exe={:?} title={:?} owner={} cloaked_shell={}",
            win.0, class, exe, title, owner, cloaked_shell
        );
    }
    println!("\n=== SUMMARY ===\nwould-manage: {managed}\nwould-skip:   {skipped}");

    Ok(())
}

pub fn run_daemon() -> Result<()> {
    // Held for the entire daemon lifetime. Dropping the guard at the end
    // of `run_daemon` releases the named mutex so a subsequent launch can
    // start immediately.
    let _instance_guard = runtime::ensure_single_instance()?;

    runtime::install_panic_hook();
    dwmend_platform::dpi::set_per_monitor_v2()?;
    dwmend_platform::foreground::allow_set_foreground(5)?;

    // ---- config ----------------------------------------------------------
    let config_path = config::default_path()?;
    if config::ensure_default(&config_path)? {
        tracing::info!(path = %config_path.display(), "wrote default config");
    }
    let cfg = config::Config::load(&config_path)?;
    let mut rules = cfg.compile_rules()?;
    let bar_height = if cfg.general.bar_enabled {
        ui::bar::DEFAULT_HEIGHT
    } else {
        0
    };
    let gaps = compose_gaps(&cfg, bar_height);
    let border_color = parse_focus_color(&cfg);

    // ---- platform listeners ----------------------------------------------
    let winevent_rx = dwmend_platform::winevent::start()?;
    let display_rx = dwmend_platform::display_change::start()?;

    // ---- focus border overlay -------------------------------------------
    if cfg.general.border_width > 0
        && let Err(e) = dwmend_platform::focus_border::start(
            cfg.general.border_width,
            border_color,
            cfg.general.corner_radius,
        )
    {
        tracing::warn!(error = %e, "focus border init failed; continuing without thick border");
    }

    // Hidden window used as a focus target when switching to an empty
    // workspace, so apps we just hid stop receiving keystrokes.
    if let Err(e) = dwmend_platform::focus_sink::start() {
        tracing::warn!(error = %e, "focus sink init failed; empty-workspace focus may linger");
    }

    // ---- hotkey setup ----------------------------------------------------
    // Snapshot the configured keybindings now so we can detect changes on
    // reload (RegisterHotKey can't be re-registered without restarting the
    // listener thread, so we just log a "restart required" warning later).
    let original_keybindings = cfg.keybindings.clone();
    let (router, parse_errors) = hotkey::HotkeyRouter::build(&cfg.keybindings);
    for err in &parse_errors {
        tracing::warn!(%err, "ignored malformed binding");
    }
    let key_rx = dwmend_platform::keyboard::start(router.table.clone())?;

    // ---- command channel & watcher --------------------------------------
    let (cmd_tx, cmd_rx): (Sender<Command>, Receiver<Command>) = unbounded();
    reaper::start(cmd_tx.clone(), Duration::from_secs(5));
    let _watcher = watcher::start(&config_path, cmd_tx.clone())?;
    {
        let tx = cmd_tx.clone();
        ctrlc::set_handler(move || {
            let _ = tx.send(Command::Quit);
        })
        .map_err(|e| eyre!("install Ctrl-C handler: {e}"))?;
    }

    // ---- initial state ---------------------------------------------------
    let monitor_infos = dwmend_platform::monitor::enumerate()?;
    if monitor_infos.is_empty() {
        return Err(eyre!("no monitors detected"));
    }

    // ---- status bars ----------------------------------------------------
    // Start one bar per monitor BEFORE constructing the WindowManager so we
    // can reserve `bar_height` pixels at the top of every monitor's work area.
    if cfg.general.bar_enabled {
        let specs: Vec<ui::bar::BarSpec> = monitor_infos
            .iter()
            .map(|m| ui::bar::BarSpec {
                monitor_id: m.stable_id.clone(),
                bounds: m.bounds,
            })
            .collect();
        if let Err(e) = ui::bar::start(specs, bar_height, ui::bar::BarColors::default()) {
            tracing::warn!(error = %e, "status bar init failed; continuing without bar");
        }
    }

    // ---- tray icon ------------------------------------------------------
    // Failure is non-fatal — explorer.exe may not be running yet at boot,
    // or the user may have disabled notification icons entirely. Either
    // way the daemon should keep working without it.
    let tray_rx: Receiver<ui::tray::TrayAction> = match ui::tray::start(ui::tray::TrayConfig {
        config_path: config_path.clone(),
        log_dir: config::data_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    }) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!(error = %e, "tray init failed; continuing without tray");
            crossbeam_channel::never()
        }
    };

    // ---- IPC server -----------------------------------------------------
    // Listens on `\\.\pipe\DwmendDaemon-v1`. Failure is non-fatal — the
    // daemon still works, scripting from `dwmend cmd` / `dwmend query`
    // just won't be available.
    let ipc_rx_opt = match dwmend_platform::ipc::start() {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!(error = %e, "ipc server init failed; cmd/query disabled");
            None
        }
    };

    let mut wm = WindowManager::new(
        monitor_infos,
        cfg.general.workspace_count,
        gaps,
        cfg.general.on_workspace_already_visible.into(),
    )?;

    // Initial top-level scan.
    for win in dwmend_platform::enumerate::enumerate_top_level()? {
        if filter::is_manageable(win, &rules) {
            let _ = admit(&mut wm, &rules, win);
        }
    }
    let _ = wm.retile_all();
    recovery::save(&wm)?;

    let wm = Arc::new(Mutex::new(wm));

    // Push the initial bar state so workspace pills appear at startup.
    wm.lock().publish_bar_state();

    // Spawn the IPC handler thread now that we have the Arc<Mutex<WM>> and
    // cmd_tx. The handler consumes raw request lines from the pipe server
    // and turns them into commands / query snapshots.
    if let Some(ipc_rx) = ipc_rx_opt {
        ipc::start(ipc_rx, wm.clone(), cmd_tx.clone());
    }

    // Single-lock summary. Earlier this had two `wm.lock()` calls inline as
    // macro args, which deadlocked because `parking_lot::Mutex` is NOT
    // re-entrant — both guards lived until the end of the statement, so the
    // second acquire blocked forever on the same thread. The daemon never
    // reached the `select!` loop.
    {
        let guard = wm.lock();
        tracing::info!(
            monitors = guard.monitors.len(),
            windows = guard.windows.len(),
            "DWMend running"
        );
    }

    // ---- main select! loop ----------------------------------------------
    let mut last_save = std::time::Instant::now();
    let mut last_alive = std::time::Instant::now();
    let mut event_count: u64 = 0;
    let alive_tick = crossbeam_channel::tick(Duration::from_secs(10));
    loop {
        select! {
            recv(alive_tick) -> _ => {
                // Heartbeat for log-tailing diagnostics. Demoted to debug
                // because at info it produces ~8,640 lines/day of noise
                // in the rolling file appender.
                tracing::debug!(
                    event_count,
                    seconds = last_alive.elapsed().as_secs(),
                    "select! loop alive"
                );
                last_alive = std::time::Instant::now();
            }
            recv(winevent_rx) -> ev => {
                let Ok(ev) = ev else { break };
                event_count += 1;
                let mut wm = wm.lock();
                if events::handle(&mut wm, &rules, ev) {
                    wm.publish_bar_state();
                }
            }
            recv(key_rx) -> hk => {
                let Ok(hk) = hk else { break };
                event_count += 1;
                let Some(cmd) = router.command_for(hk.id) else {
                    tracing::debug!(?hk, "hotkey id with no mapped command (config reload race?)");
                    continue;
                };
                tracing::debug!(?hk.mods, vk = format!("{:#x}", hk.vk), ?cmd, "hotkey received");
                // While paused, only commands that operate on DWMend itself —
                // TogglePause / Quit / ReloadConfig — are allowed through.
                // Everything else is silently ignored so the user's normal
                // keystrokes pass to the focused app.
                let paused = dwmend_platform::keyboard::PAUSED
                    .load(std::sync::atomic::Ordering::Relaxed);
                let allow_while_paused = matches!(
                    cmd,
                    Command::TogglePause | Command::Quit | Command::ReloadConfig
                );
                if paused && !allow_while_paused {
                    tracing::debug!(?cmd, "ignored while paused");
                    continue;
                }
                match &cmd {
                    Command::Quit => {
                        let _ = cmd_tx.send(Command::Quit);
                    }
                    Command::ReloadConfig => {
                        let _ = cmd_tx.send(Command::ReloadConfig);
                    }
                    _ => {
                        let mut wm = wm.lock();
                        if let Err(e) = dispatch(&mut wm, cmd) {
                            tracing::warn!(error = %e, "command failed");
                        }
                        wm.publish_bar_state();
                    }
                }
            }
            recv(display_rx) -> _ => {
                event_count += 1;
                tracing::info!("display topology changed; reconciling");
                let infos = match dwmend_platform::monitor::enumerate() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "monitor enumerate failed");
                        continue;
                    }
                };
                let mut wm = wm.lock();
                if let Err(e) = wm.reconcile_monitors(infos) {
                    tracing::warn!(error = %e, "reconcile_monitors failed");
                }
                wm.publish_bar_state();
            }
            recv(tray_rx) -> ev => {
                let Ok(action) = ev else { break };
                event_count += 1;
                // Tray actions translate 1:1 to the equivalent daemon
                // command; we route through cmd_tx so the existing
                // Quit / ReloadConfig handlers in the cmd_rx arm pick
                // them up unchanged.
                let cmd = match action {
                    ui::tray::TrayAction::TogglePause => Command::TogglePause,
                    ui::tray::TrayAction::ReloadConfig => Command::ReloadConfig,
                    ui::tray::TrayAction::Quit => Command::Quit,
                };
                tracing::info!(?action, "tray action received");
                let _ = cmd_tx.send(cmd);
            }
            recv(cmd_rx) -> cmd => {
                let Ok(cmd) = cmd else { break };
                event_count += 1;
                match cmd {
                    Command::Quit => {
                        tracing::info!("Quit received; shutting down");
                        break;
                    }
                    Command::ReloadConfig | Command::ConfigChanged => {
                        match config::Config::load(&config_path) {
                            Ok(new_cfg) => match new_cfg.compile_rules() {
                                Ok(new_rules) => {
                                    // NOTE: hotkey bindings cannot be hot-
                                    // reloaded with `RegisterHotKey` — the
                                    // listener thread would have to be torn
                                    // down and respawned. For v1 we apply
                                    // gap/rule changes immediately and tell
                                    // the user that key changes need a restart.
                                    if new_cfg.keybindings != original_keybindings {
                                        tracing::warn!(
                                            "keybinding changes detected; restart dwmend to apply them"
                                        );
                                    }
                                    rules = new_rules;
                                    let mut wm = wm.lock();
                                    // IMPORTANT: re-add the bar's reserved
                                    // height. Earlier this dropped it and
                                    // every workspace switch after the first
                                    // reload tiled under the bar.
                                    wm.gaps = compose_gaps(&new_cfg, bar_height);
                                    dwmend_platform::focus_border::set_color(parse_focus_color(&new_cfg));
                                    dwmend_platform::focus_border::set_width(new_cfg.general.border_width);
                                    dwmend_platform::focus_border::set_radius(new_cfg.general.corner_radius);
                                    wm.on_already_visible =
                                        new_cfg.general.on_workspace_already_visible.into();
                                    let _ = wm.retile_all();
                                    wm.publish_bar_state();
                                    tracing::info!("config reloaded");
                                }
                                Err(e) => tracing::error!(error = %e, "bad rules; keeping old"),
                            },
                            Err(e) => tracing::error!(error = %e, "config reload failed"),
                        }
                    }
                    Command::Reap => {
                        let mut wm = wm.lock();
                        let dead: Vec<_> = wm
                            .windows
                            .keys()
                            .copied()
                            .filter(|id| !dwmend_platform::window::Window(id.0).is_alive())
                            .collect();
                        for id in dead {
                            let _ = wm.unmanage(id);
                        }
                        wm.publish_bar_state();
                    }
                    other => {
                        let mut wm = wm.lock();
                        if let Err(e) = dispatch(&mut wm, other) {
                            tracing::warn!(error = %e, "command failed");
                        }
                        wm.publish_bar_state();
                    }
                }
            }
        }

        if last_save.elapsed() > Duration::from_secs(5) {
            // Snapshot the id list under a SHORT critical section so the
            // (potentially slow) disk write does not block events / hotkeys.
            // The atomic-rename in `save_ids` can stall arbitrarily on
            // OneDrive sync, antivirus scan, or a network home directory.
            let ids: Vec<isize> = {
                let g = wm.lock();
                g.windows.keys().map(|w| w.0).collect()
            };
            let _ = recovery::save_ids(&ids);
            last_save = std::time::Instant::now();
        }
    }

    // ---- shutdown --------------------------------------------------------
    tracing::info!("restoring visibility of all managed windows");
    {
        let mut wm = wm.lock();
        wm.restore_all_managed_windows();
    }
    dwmend_platform::focus_border::stop();
    ui::bar::stop();
    ui::tray::stop();
    dwmend_platform::keyboard::stop();
    dwmend_platform::winevent::stop();
    let _ = recovery::clear();
    tracing::info!("DWMend exited cleanly");
    Ok(())
}

// ---- helpers ---------------------------------------------------------------

/// Build a `Gaps` from the config plus the bar's reserved pixel height.
/// Used at startup AND on config reload so the bar reservation is never lost.
fn compose_gaps(cfg: &config::Config, bar_height: i32) -> Gaps {
    Gaps {
        inner: cfg.general.gap,
        top: cfg.general.outer_gap_top + bar_height,
        right: cfg.general.outer_gap_right,
        bottom: cfg.general.outer_gap_bottom,
        left: cfg.general.outer_gap_left,
    }
}

/// Resolve the focused-overlay colour from config. Bad colour strings log a
/// warning and fall back to a bright sky-blue so a typo never blocks startup.
fn parse_focus_color(cfg: &config::Config) -> u32 {
    const FALLBACK: u32 = dwmend_platform::dwm::rgb(0x4F, 0xC3, 0xF7);
    config::parse_border_color(&cfg.general.focused_border_color).unwrap_or_else(|e| {
        tracing::warn!(
            value = %cfg.general.focused_border_color,
            error = %e,
            "bad focused_border_color; using default sky-blue"
        );
        FALLBACK
    })
}
