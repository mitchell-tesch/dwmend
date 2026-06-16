//! Subcommand handlers that don't need the full daemon — IPC client
//! helpers (`dwmend cmd …`, `dwmend query …`), `dwmend autostart`, and
//! the `help` printer.

use crate::autostart;
use color_eyre::Result;

/// `dwmend cmd <action string>` — send a single command to the running
/// daemon via the named pipe and print the JSON response. Action grammar
/// is the same as the `[keybindings]` config section.
pub fn run_cmd(action: &str) -> Result<()> {
    let action = action.trim();
    if action.is_empty() {
        eprintln!("dwmend cmd: missing action (e.g. `dwmend cmd \"focus left\"`)");
        std::process::exit(2);
    }
    let req = serde_json::json!({ "kind": "cmd", "action": action });
    let resp = dwmend_platform::ipc::client_send(&req.to_string())?;
    print_response(&resp)
}

/// `dwmend query <topic>` — ask the daemon for a JSON snapshot.
/// Topics: `state`, `monitors`, `workspaces`, `focused`, `ping`.
pub fn run_query(topic: &str) -> Result<()> {
    let topic = topic.trim();
    if topic.is_empty() {
        eprintln!("dwmend query: missing topic (state | monitors | workspaces | focused | ping)");
        std::process::exit(2);
    }
    let req = serde_json::json!({ "kind": "query", "topic": topic });
    let resp = dwmend_platform::ipc::client_send(&req.to_string())?;
    print_response(&resp)
}

/// Pretty-print a JSON response and exit non-zero if the daemon reported
/// `ok = false`.
fn print_response(resp: &str) -> Result<()> {
    let parsed: serde_json::Value =
        serde_json::from_str(resp.trim()).unwrap_or(serde_json::Value::String(resp.into()));
    let pretty = serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| resp.to_string());
    println!("{pretty}");
    if parsed.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        std::process::exit(1);
    }
    Ok(())
}

/// Handle `dwmend autostart {enable|disable|status}`.
///
/// Writes / reads / clears an HKCU Run entry pointing at the currently
/// running `dwmend.exe`. Per-user means no elevation; uninstalling is one
/// `dwmend autostart disable` away.
pub fn run_autostart(action: &str) -> Result<()> {
    match action {
        "enable" | "on" => {
            let path = autostart::enable()?;
            println!("autostart enabled: {}", path.display());
            Ok(())
        }
        "disable" | "off" => {
            autostart::disable()?;
            println!("autostart disabled");
            Ok(())
        }
        "status" | "" => {
            match autostart::status()? {
                Some(path) => println!("autostart enabled: {}", path.display()),
                None => println!("autostart disabled"),
            }
            Ok(())
        }
        other => {
            eprintln!("dwmend autostart: unknown action `{other}` (try enable | disable | status)");
            std::process::exit(2);
        }
    }
}

pub fn print_help() {
    println!(
        "dwmend — Windows 11 tiling window manager\n\n\
        USAGE:\n  \
            dwmend                       Run the daemon (default).\n  \
            dwmend dryrun                Print what DWMend would manage without moving any.\n  \
            dwmend restore               Restore visibility of windows DWMend hid before a crash.\n  \
            dwmend cmd \"<action>\"       Send a command to the running daemon (e.g. \"focus left\").\n  \
            dwmend query <topic>         Print daemon state as JSON (state|monitors|workspaces|focused|ping).\n  \
            dwmend autostart enable      Register DWMend to launch on sign-in (HKCU Run key).\n  \
            dwmend autostart disable     Remove the autostart entry.\n  \
            dwmend autostart status      Show whether autostart is enabled.\n  \
            dwmend help                  Show this message.\n"
    );
}
