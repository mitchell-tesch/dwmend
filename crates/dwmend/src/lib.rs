//! DWMend — Windows 11 tiling window manager (library crate).
//!
//! The `dwmend` binary is a thin shim over [`run`]; everything else lives
//! here so integration tests, debug tooling, or future binaries can pull
//! in pieces of the daemon without re-implementing CLI dispatch.

pub mod autostart;
pub mod cli;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod events;
pub mod filter;
pub mod hotkey;
pub mod ids;
pub mod ipc;
pub mod monitor;
pub mod reaper;
pub mod recovery;
pub mod runtime;
pub mod state;
pub mod ui;
pub mod watcher;
pub mod window;
pub mod workspace;

use color_eyre::Result;

/// Top-level CLI dispatcher. Parses `std::env::args()` and routes to the
/// daemon, the IPC client subcommands, the recovery / dryrun / autostart
/// helpers, or the help printer.
///
/// Bootstrap order:
///  1. Install `color_eyre`
///  2. Initialise tracing (rolling file + stderr)
///  3. Dispatch on the first positional argument
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("");

    color_eyre::install()?;
    let _guard = runtime::setup_tracing()?;

    match subcommand {
        "" => daemon::run_daemon(),
        "restore" => recovery::run_restore(),
        "dryrun" => daemon::run_dryrun(),
        "autostart" => cli::run_autostart(args.get(1).map(|s| s.as_str()).unwrap_or("")),
        "cmd" => cli::run_cmd(&args[1..].join(" ")),
        "query" => cli::run_query(args.get(1).map(|s| s.as_str()).unwrap_or("")),
        "help" | "-h" | "--help" => {
            cli::print_help();
            Ok(())
        }
        other => {
            eprintln!("dwmend: unknown subcommand `{other}`. Try `dwmend help`.");
            std::process::exit(2);
        }
    }
}
