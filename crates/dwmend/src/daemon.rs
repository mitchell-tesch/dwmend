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
    // Bar height is captured here and reused for the lifetime of the
    // daemon. Hot-reloading the height would require resizing every bar
    // HWND AND recomposing every workspace's gaps reservation, which we
    // defer to a daemon restart with a warning (see the reload arm).
    let bar_height = if cfg.general.bar_enabled {
        cfg.bar.height.max(16)
    } else {
        0
    };
    let original_bar_height = bar_height;
    let gaps = compose_gaps(&cfg, bar_height);
    let border_color = parse_focus_color(&cfg);
    let bar_colors = parse_bar_colors(&cfg.bar);
    let bar_segments = bar_segments_from_cfg(&cfg.bar);

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
    // reload. Both `router` and `original_keybindings` need `mut` because
    // a successful `keyboard::restart` swaps in a new router and the
    // tracked baseline (so subsequent reloads compare against the actual
    // active set, not the startup snapshot).
    let mut original_keybindings = cfg.keybindings.clone();
    let (mut router, parse_errors) = hotkey::HotkeyRouter::build(&cfg.keybindings);
    for err in &parse_errors {
        tracing::warn!(%err, "ignored malformed binding");
    }
    let key_rx = dwmend_platform::keyboard::start(router.table.clone())?;

    // ---- command channel & watcher --------------------------------------
    let (cmd_tx, cmd_rx): (Sender<Command>, Receiver<Command>) = unbounded();
    // Publish the sender to the bar so pill clicks can switch workspaces.
    // Done before bar startup so the very first click after the bar is
    // shown is dispatched correctly.
    ui::bar::set_command_tx(cmd_tx.clone());
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
        if let Err(e) = ui::bar::start(specs, bar_height, bar_colors) {
            tracing::warn!(error = %e, "status bar init failed; continuing without bar");
        } else {
            ui::bar::set_segments(bar_segments);
        }
    }

    // ---- notifications --------------------------------------------------
    // Failure is non-fatal: a missing toast subsystem only loses the
    // pop-up feedback; everything still logs as before.
    let toast_cfg = parse_toast_config(&cfg.notifications);
    let toast_specs = build_toast_specs(&monitor_infos, bar_height);
    if let Err(e) = ui::toast::start(toast_specs, toast_cfg) {
        tracing::warn!(error = %e, "toast subsystem init failed; logs are the only feedback");
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
    // Apply the configured layout to every workspace before the initial
    // top-level scan so admitted windows split using the right mode from
    // their very first insert.
    wm.apply_layout_mode_all(map_layout(cfg.general.layout));

    // Initial top-level scan.
    for win in dwmend_platform::enumerate::enumerate_top_level()? {
        if filter::is_manageable(win, &rules) {
            let _ = admit(&mut wm, &rules, win);
        }
    }
    let _ = wm.retile_all();
    recovery::save(&wm)?;
    // Cache of the last sorted id-list we wrote to `hwnds.json`. Used by
    // the periodic save in the select! loop to skip redundant disk writes
    // when the managed-window set hasn't changed since the previous tick
    // — important when `%LOCALAPPDATA%` is on a network home / OneDrive
    // path where atomic-rename can stall arbitrarily.
    let mut last_saved_ids: Vec<isize> = wm.windows.keys().map(|w| w.0).collect();
    last_saved_ids.sort_unstable();

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
                // Resync the toast subsystem to the new topology BEFORE
                // the WM lock is taken so toasts emitted during the
                // reconcile can target the correct monitor list.
                ui::toast::sync_monitors(build_toast_specs(&infos, bar_height));
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
                                    // Hot-reload `[keybindings]` by tearing
                                    // down the listener and respawning it
                                    // with a fresh hotkey table. The
                                    // EVENT_TX channel survives the
                                    // restart, so `key_rx` keeps working.
                                    if new_cfg.keybindings != original_keybindings {
                                        let (new_router, parse_errors) =
                                            hotkey::HotkeyRouter::build(&new_cfg.keybindings);
                                        for err in &parse_errors {
                                            tracing::warn!(%err,
                                                "ignored malformed binding in reloaded config");
                                        }
                                        match dwmend_platform::keyboard::restart(
                                            new_router.table.clone(),
                                        ) {
                                            Ok(()) => {
                                                let n = new_router.id_to_command.len();
                                                router = new_router;
                                                original_keybindings = new_cfg.keybindings.clone();
                                                tracing::info!(
                                                    count = n,
                                                    "keybindings hot-reloaded"
                                                );
                                                ui::toast::show(
                                                    ui::toast::ToastLevel::Info,
                                                    format!("Bindings reloaded ({n})"),
                                                );
                                            }
                                            Err(e) => {
                                                tracing::error!(error = %e,
                                                    "keybinding hot-reload failed; \
                                                     old bindings still active");
                                                ui::toast::show(
                                                    ui::toast::ToastLevel::Error,
                                                    "Keybinding reload failed".to_string(),
                                                );
                                            }
                                        }
                                    }
                                    // Bar height is baked into the gap
                                    // composer at startup; resizing every
                                    // bar HWND on the fly is a future
                                    // enhancement. Warn so config edits
                                    // don't silently no-op.
                                    let new_bar_h = if new_cfg.general.bar_enabled {
                                        new_cfg.bar.height.max(16)
                                    } else {
                                        0
                                    };
                                    if new_bar_h != original_bar_height {
                                        tracing::warn!(
                                            old = original_bar_height, new = new_bar_h,
                                            "bar height change detected; restart dwmend to apply"
                                        );
                                        ui::toast::show(
                                            ui::toast::ToastLevel::Warn,
                                            "Bar height changed; restart to apply".to_string(),
                                        );
                                    }
                                    rules = new_rules;
                                    let mut wm = wm.lock();
                                    // IMPORTANT: re-add the bar's reserved
                                    // height. Earlier this dropped it and
                                    // every workspace switch after the first
                                    // reload tiled under the bar.
                                    wm.gaps = compose_gaps(&new_cfg, bar_height);
                                    // Re-apply the configured layout mode
                                    // to every workspace. Runtime
                                    // `toggle_layout` overrides are
                                    // intentionally lost on reload \u2014 the
                                    // config file is the source of truth
                                    // for the desired baseline.
                                    wm.apply_layout_mode_all(map_layout(new_cfg.general.layout));
                                    dwmend_platform::focus_border::set_color(parse_focus_color(&new_cfg));
                                    dwmend_platform::focus_border::set_width(new_cfg.general.border_width);
                                    dwmend_platform::focus_border::set_radius(new_cfg.general.corner_radius);
                                    // Bar theme + segments are hot-reloadable.
                                    ui::bar::set_colors(parse_bar_colors(&new_cfg.bar));
                                    ui::bar::set_segments(bar_segments_from_cfg(&new_cfg.bar));
                                    // Toast subsystem: colours, TTL,
                                    // concurrency cap, and the
                                    // master `enabled` switch flow
                                    // straight through. In-flight
                                    // toasts complete with the
                                    // values they were spawned under.
                                    ui::toast::set_config(parse_toast_config(&new_cfg.notifications));
                                    wm.on_already_visible =
                                        new_cfg.general.on_workspace_already_visible.into();
                                    let _ = wm.retile_all();
                                    wm.publish_bar_state();
                                    tracing::info!("config reloaded");
                                    ui::toast::show(
                                        ui::toast::ToastLevel::Info,
                                        "Config reloaded".to_string(),
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "bad rules; keeping old");
                                    ui::toast::show(
                                        ui::toast::ToastLevel::Error,
                                        format!("Config rejected: {e}"),
                                    );
                                }
                            },
                            Err(e) => {
                                tracing::error!(error = %e, "config reload failed");
                                ui::toast::show(
                                    ui::toast::ToastLevel::Error,
                                    format!("Config reload failed: {e}"),
                                );
                            }
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
            //
            // Skip the write entirely when the managed-window set is
            // identical to the last saved snapshot. The vec compare runs in
            // a few microseconds for typical desktops (<100 windows); the
            // disk path it gates is orders of magnitude more expensive.
            let mut ids: Vec<isize> = {
                let g = wm.lock();
                g.windows.keys().map(|w| w.0).collect()
            };
            ids.sort_unstable();
            if ids != last_saved_ids && recovery::save_ids(&ids).is_ok() {
                last_saved_ids = ids;
            }
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
    ui::toast::stop();
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

/// Build a `BarColors` from the `[bar.colors]` config table. Each field is
/// parsed independently so a single typo only fails its own slot \u2014 the
/// rest of the bar still reflects the user's edits.
fn parse_bar_colors(cfg: &config::Bar) -> ui::bar::BarColors {
    let defaults = ui::bar::BarColors::default();
    let parse = |name: &str, s: &str, fallback: u32| {
        config::parse_border_color(s).unwrap_or_else(|e| {
            tracing::warn!(field = name, value = s, error = %e,
                "bad bar color; using default");
            fallback
        })
    };
    ui::bar::BarColors {
        background: parse("background", &cfg.colors.background, defaults.background),
        foreground: parse("foreground", &cfg.colors.foreground, defaults.foreground),
        active_bg: parse("active_bg", &cfg.colors.active_bg, defaults.active_bg),
        active_fg: parse("active_fg", &cfg.colors.active_fg, defaults.active_fg),
        visible_outline: parse(
            "visible_outline",
            &cfg.colors.visible_outline,
            defaults.visible_outline,
        ),
        dim_fg: parse("dim_fg", &cfg.colors.dim_fg, defaults.dim_fg),
    }
}

/// Translate `[bar]` segment toggles into the bar crate's `BarSegments`.
fn bar_segments_from_cfg(cfg: &config::Bar) -> ui::bar::BarSegments {
    ui::bar::BarSegments {
        icon: cfg.show_icon,
        workspaces: cfg.show_workspaces,
        focused_title: cfg.show_focused_title,
        clock: cfg.show_clock,
        battery: cfg.show_battery,
        network: cfg.show_network,
        pause_indicator: cfg.show_pause_indicator,
    }
}

/// TOML `general.layout` \u2192 layout-engine enum. Kept in `daemon.rs` so
/// `dwmend-layout` doesn't need to know about serde / the wire format.
fn map_layout(k: config::LayoutKind) -> dwmend_layout::bsp::LayoutMode {
    match k {
        config::LayoutKind::Dwindle => dwmend_layout::bsp::LayoutMode::Dwindle,
        config::LayoutKind::Spiral => dwmend_layout::bsp::LayoutMode::Spiral,
    }
}
/// Build a `ToastConfig` from the `[notifications]` config table.
/// Bad colour strings fall back to the toast subsystem's defaults
/// with a per-field warning so a typo never blocks startup.
fn parse_toast_config(cfg: &config::Notifications) -> ui::toast::ToastConfig {
    let defaults = ui::toast::ToastColors::default();
    let parse = |name: &str, s: &str, fallback: u32| {
        config::parse_border_color(s).unwrap_or_else(|e| {
            tracing::warn!(field = name, value = s, error = %e,
                "bad notifications color; using default");
            fallback
        })
    };
    ui::toast::ToastConfig {
        enabled: cfg.enabled,
        ttl_ms: cfg.ttl_ms,
        max_concurrent: cfg.max_concurrent.max(1),
        anchor: match cfg.anchor {
            config::NotificationAnchor::TopRight => ui::toast::ToastAnchor::TopRight,
        },
        colors: ui::toast::ToastColors {
            info_bg: parse("info_bg", &cfg.colors.info_bg, defaults.info_bg),
            info_fg: parse("info_fg", &cfg.colors.info_fg, defaults.info_fg),
            warn_bg: parse("warn_bg", &cfg.colors.warn_bg, defaults.warn_bg),
            warn_fg: parse("warn_fg", &cfg.colors.warn_fg, defaults.warn_fg),
            error_bg: parse("error_bg", &cfg.colors.error_bg, defaults.error_bg),
            error_fg: parse("error_fg", &cfg.colors.error_fg, defaults.error_fg),
        },
    }
}

/// Build per-monitor `ToastSpec`s from a fresh `MonitorInfo` list and
/// the current bar height. Toast `work_area` starts BELOW the bar
/// reservation so toasts and the clock zone never overlap.
fn build_toast_specs(
    infos: &[dwmend_platform::monitor::MonitorInfo],
    bar_height: i32,
) -> Vec<ui::toast::ToastSpec> {
    infos
        .iter()
        .map(|m| {
            let mut wa = m.work_area;
            // Subtract the bar reservation from the top of the work
            // area. `work_area` already excludes the OS taskbar; we
            // additionally exclude DWMend's bar so the toast never
            // covers the focused-title or clock zones.
            if bar_height > 0 && wa.h > bar_height {
                wa.y += bar_height;
                wa.h -= bar_height;
            }
            ui::toast::ToastSpec {
                monitor_id: m.stable_id.clone(),
                work_area: wa,
            }
        })
        .collect()
}