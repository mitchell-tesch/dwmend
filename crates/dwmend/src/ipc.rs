//! Daemon-side IPC: JSON request/response over the named pipe.
//!
//! The platform crate's `ipc` module owns the pipe; this module owns the
//! protocol. We run a dedicated handler thread that drains
//! `Receiver<IpcRequest>`, parses each line as a JSON request, performs
//! the action (dispatching a `Command` or building a query snapshot from
//! the locked `WindowManager`), serialises the response, and sends it
//! back through the request's one-shot reply channel.
//!
//! ## Protocol
//!
//! All messages are single-line UTF-8 JSON.
//!
//! Requests are tagged by `"kind"`:
//!
//! * `{"kind":"cmd","action":"<action string>"}` — same grammar as the
//!   `[keybindings]` section of `config.toml`. Reply: `{"ok":true}` or
//!   `{"ok":false,"error":"..."}`.
//! * `{"kind":"query","topic":"<topic>"}` — read-only snapshot. Topics
//!   are `state` (everything), `monitors`, `workspaces`, `focused`, and
//!   `ping`. Reply: `{"ok":true,"data":...}`.
//!
//! Anything malformed gets `{"ok":false,"error":"..."}`.

use crate::commands::Command;
use crate::hotkey::parse_action;
use crate::state::WindowManager;
use crossbeam_channel::Sender;
use dwmend_platform::ipc::IpcRequest;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ---- request / response shapes --------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Request {
    Cmd { action: String },
    Query { topic: String },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Response {
    Ok { ok: bool },
    OkData { ok: bool, data: serde_json::Value },
    Err { ok: bool, error: String },
}

impl Response {
    fn ok() -> Self {
        Self::Ok { ok: true }
    }
    fn ok_data(v: serde_json::Value) -> Self {
        Self::OkData { ok: true, data: v }
    }
    fn err<E: std::fmt::Display>(e: E) -> Self {
        Self::Err {
            ok: false,
            error: e.to_string(),
        }
    }
}

// ---- query view types -----------------------------------------------------

#[derive(Debug, Serialize)]
struct StateView {
    paused: bool,
    monitors: Vec<MonitorView>,
    workspaces: Vec<WorkspaceView>,
    focused: Option<FocusedView>,
}

#[derive(Debug, Serialize)]
struct MonitorView {
    id: String,
    primary: bool,
    dpi: u32,
    bounds: RectView,
    work_area: RectView,
    current_workspace: u32,
}

#[derive(Debug, Serialize)]
struct WorkspaceView {
    id: u32,
    monitor_id: Option<String>,
    monocle: bool,
    window_count: usize,
    windows: Vec<WindowView>,
}

#[derive(Debug, Serialize)]
struct WindowView {
    hwnd: isize,
    title: String,
    class: String,
    exe: String,
    mode: &'static str,
}

#[derive(Debug, Serialize)]
struct FocusedView {
    monitor_id: String,
    workspace: u32,
    hwnd: Option<isize>,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct RectView {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl From<dwmend_platform::Rect> for RectView {
    fn from(r: dwmend_platform::Rect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }
    }
}

// ---- public entry point ----------------------------------------------------

/// Spawn the JSON handler thread. Reads `IpcRequest`s from the platform
/// pipe server, dispatches commands or builds query snapshots, sends the
/// JSON response back via the request's reply channel.
pub fn start(
    rx: crossbeam_channel::Receiver<IpcRequest>,
    wm: Arc<Mutex<WindowManager>>,
    cmd_tx: Sender<Command>,
) {
    std::thread::Builder::new()
        .name("dwmend-ipc-handler".into())
        .spawn(move || {
            tracing::info!("ipc handler started");
            while let Ok(req) = rx.recv() {
                let response = process(&req.line, &wm, &cmd_tx);
                let serialised = serde_json::to_string(&response)
                    .unwrap_or_else(|e| format!(r#"{{"ok":false,"error":"serialize: {e}"}}"#));
                let _ = req.reply.try_send(serialised);
            }
            tracing::info!("ipc handler exiting");
        })
        // Best effort — losing the handler is non-fatal; the daemon keeps
        // running and the pipe server will time out individual requests.
        .ok();
}

fn process(line: &str, wm: &Mutex<WindowManager>, cmd_tx: &Sender<Command>) -> Response {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Response::err("empty request");
    }
    let req: Request = match serde_json::from_str(trimmed) {
        Ok(r) => r,
        Err(e) => return Response::err(format!("parse: {e}")),
    };
    match req {
        Request::Cmd { action } => match parse_action(&action) {
            Ok(cmd) => match cmd_tx.send(cmd) {
                Ok(()) => Response::ok(),
                Err(e) => Response::err(format!("send: {e}")),
            },
            Err(e) => Response::err(format!("action: {e}")),
        },
        Request::Query { topic } => match topic.as_str() {
            "ping" => Response::ok_data(serde_json::json!("pong")),
            "state" => match build_state(wm) {
                Ok(v) => Response::ok_data(v),
                Err(e) => Response::err(e),
            },
            "monitors" => match build_monitors(wm) {
                Ok(v) => Response::ok_data(v),
                Err(e) => Response::err(e),
            },
            "workspaces" => match build_workspaces(wm) {
                Ok(v) => Response::ok_data(v),
                Err(e) => Response::err(e),
            },
            "focused" => match build_focused(wm) {
                Ok(v) => Response::ok_data(v),
                Err(e) => Response::err(e),
            },
            other => Response::err(format!("unknown topic `{other}`")),
        },
    }
}

// ---- builders --------------------------------------------------------------

fn build_state(wm: &Mutex<WindowManager>) -> Result<serde_json::Value, String> {
    let g = wm.lock();
    let monitors = monitors_view(&g);
    let workspaces = workspaces_view(&g);
    let focused = focused_view(&g);
    let view = StateView {
        paused: g.paused,
        monitors,
        workspaces,
        focused,
    };
    serde_json::to_value(view).map_err(|e| format!("serialize: {e}"))
}

fn build_monitors(wm: &Mutex<WindowManager>) -> Result<serde_json::Value, String> {
    let g = wm.lock();
    serde_json::to_value(monitors_view(&g)).map_err(|e| format!("serialize: {e}"))
}

fn build_workspaces(wm: &Mutex<WindowManager>) -> Result<serde_json::Value, String> {
    let g = wm.lock();
    serde_json::to_value(workspaces_view(&g)).map_err(|e| format!("serialize: {e}"))
}

fn build_focused(wm: &Mutex<WindowManager>) -> Result<serde_json::Value, String> {
    let g = wm.lock();
    serde_json::to_value(focused_view(&g)).map_err(|e| format!("serialize: {e}"))
}

fn monitors_view(g: &WindowManager) -> Vec<MonitorView> {
    g.monitors
        .values()
        .map(|m| MonitorView {
            id: m.id.0.clone(),
            primary: m.info.primary,
            dpi: m.info.dpi,
            bounds: m.info.bounds.into(),
            work_area: m.info.work_area.into(),
            current_workspace: m.current_workspace.0,
        })
        .collect()
}

fn workspaces_view(g: &WindowManager) -> Vec<WorkspaceView> {
    use std::collections::HashMap;
    // Index windows by workspace for one O(n) pass instead of N×O(n).
    let mut by_ws: HashMap<u32, Vec<WindowView>> = HashMap::new();
    for w in g.windows.values() {
        by_ws.entry(w.workspace.0).or_default().push(WindowView {
            hwnd: w.id.0,
            title: w.title.clone(),
            class: w.class.clone(),
            exe: w.exe_name.clone(),
            mode: match w.mode {
                crate::window::WindowMode::Tiled => "tiled",
                crate::window::WindowMode::Floating => "floating",
            },
        });
    }
    g.workspaces
        .values()
        .map(|ws| {
            let windows = by_ws.remove(&ws.id.0).unwrap_or_default();
            WorkspaceView {
                id: ws.id.0,
                monitor_id: ws.active_monitor.as_ref().map(|m| m.0.clone()),
                monocle: ws.monocle,
                window_count: windows.len(),
                windows,
            }
        })
        .collect()
}

fn focused_view(g: &WindowManager) -> Option<FocusedView> {
    let mid = g.focused_monitor.as_ref()?;
    let m = g.monitors.get(mid)?;
    let ws = m.current_workspace;
    let (hwnd, title) = match g.focused_window.and_then(|w| g.windows.get(&w)) {
        Some(mw) => (Some(mw.id.0), Some(mw.title.clone())),
        None => (None, None),
    };
    Some(FocusedView {
        monitor_id: mid.0.clone(),
        workspace: ws.0,
        hwnd,
        title,
    })
}
