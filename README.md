<p align="center">
  <img src="assets/icon.png" alt="DWMend logo" width="128" height="128" />
</p>

# DWMend — Tiling Window Manager for Windows 11

[![CI](https://github.com/mitchell-tesch/dwmend/actions/workflows/ci.yml/badge.svg)](https://github.com/mitchell-tesch/dwmend/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Platform: Windows 11](https://img.shields.io/badge/platform-Windows%2011-0078d6)
![MSRV](https://img.shields.io/badge/rustc-1.85%2B-orange?logo=rust)

A lightweight (and opinionated) tiling window manager for Windows 11, inspired by
[Hyprland](https://hyprland.org/) and [komorebi](https://github.com/LGUG2Z/komorebi). A mend for the DWM.

## Features

- **Lightweight** — single daemon binary.
- **No elevation, no installer** — run as a user without elevated Administration permissions. `dwmend autostart enable` writes an HKCU `Run` entry so the daemon launches on next sign-in. `dwmend autostart disable` removes it.
- **Dynamic tiling** — Binary Space Partition (BSP) approach. Two layout modes: **dwindle** (aspect-ratio splits, default) and **spiral** (alternating axis splits). Switch globally via `[general] layout = "spiral"` in config or per-workspace via the `toggle_layout` action.
- **Global workspace pool** — 10 named workspaces; `Alt+#` (where # is the workspace number) brings a workspace to the focused monitor (swapping it from whichever monitor was showing it). `Alt+Shift+N` moves the focused window *and* follows focus to the target workspace.
- **Status bar** — thin GDI bar on each monitor showing workspace pills (active / visible-elsewhere / occupied / empty), the focused window title centred on the bar and a clock / battery / network indicators on the right. **Click a pill to switch that monitor's workspace.** Colours, height, and per-segment toggles are configurable under `[bar]`.
- **Active tile focus border** — thick configurable-colour overlay frame around the active tile.
- **Stack containers** — group multiple windows into one tile. `Alt+G` is smart: with two tiles side-by-side it merges them into a stack on the first press; with a single tile it converts it to a 1-member stack so subsequent windows pile in. `Alt+[` / `Alt+]` cycle through. `Alt+Shift+G` pops the focused member back out into its own tile. The bar shows a `[2/3]` indicator next to the focused title when applicable.
- **Floating window keyboard control** — `Alt+Shift+H/J/K/L` translates the focused float by 32 px; `Alt+Ctrl+H/J/K/L` resizes the matching edge by 32 px.
- **Window rules** — match by `exe` / `class` / `title` regex; actions are `ignore`, `float`, `tile`, or `workspace = N` to pin matching apps to a specific workspace at launch (without stealing the user's focus).
- **Window peek** — `Alt+E` opens a sticky-mode picker overlaying live DWM thumbnails of every window on the focused workspace. Cycle with your normal `Alt+H/L` focus keys (the daemon intercepts them while peek is up); `Alt+Enter` commits, `Alt+E` again dismisses. Configurable under `[peek]`.
- **Notification toasts** — transient pop-ups for config reloads, keybinding failures, pause toggles, and layout mode flips. Click-through, never steal focus. Configurable under `[notifications]`. Scripts can fire their own via `dwmend cmd "notify info 'Build complete'"`.
- **TOML config + hot reload** — gaps, colours, rules, **keybindings**, bar theme/segments, **notification colours/TTL**, **peek theme** all update on save. Bar height changes still need a daemon restart.
- **IPC** — named-pipe server at `\\.\pipe\DwmendDaemon-v1`. Use `dwmend cmd "focus left"` from PowerShell / AutoHotKey / StreamDeck to drive the daemon, or `dwmend query state` to introspect.
- **Crash recovery** — `dwmend.exe restore` makes any windows **DWMend** hid visible again if the daemon crashes or is force-killed.

## Install

Two distribution options. Both are user-mode — neither needs administrator rights.

**MSI installer** (recommended for most users):

1. Download `dwmend.msi` from the [latest release](https://github.com/mitchell-tesch/dwmend/releases/latest).
2. Double-click. Installs to `%LOCALAPPDATA%\Programs\dwmend\` and adds a Start Menu shortcut. No UAC prompt; uninstalls cleanly via *Settings → Apps*.
3. Silent install for scripts: `msiexec /i dwmend.msi /qn`.

**Portable executable** (no installer):

1. Download `dwmend.exe` from the [latest release](https://github.com/mitchell-tesch/dwmend/releases/latest).
2. Drop it anywhere on disk and run it.

Both artifacts ship with a matching `.sha256` for verification:

```pwsh
(Get-FileHash -Algorithm SHA256 .\dwmend.msi).Hash.ToLower()
# compare against the contents of dwmend.msi.sha256
```

## Build

Requires:
- Rust stable on `x86_64-pc-windows-msvc`
- Visual Studio Build Tools or VS with the C++ workload + Windows 11 SDK
- A Developer PowerShell (`Launch-VsDevShell.ps1 -Arch amd64`)

```pwsh
cargo build --release
```

The release binary at `target\release\dwmend.exe` (~3 MB).

To build the MSI locally (requires the [WiX Toolset 3.14](https://wixtoolset.org/docs/wix3/) on PATH and `cargo install cargo-wix`):

```pwsh
cargo build --workspace --release

# Stage the binder inputs into target\wix\ \u2014 cargo-wix runs light.exe
# from there and doesn't add the wxs source dir as a binder path, so
# License.rtf / dwmend.ico must be alongside the linker's CWD.
New-Item -ItemType Directory -Force -Path target\wix | Out-Null
Copy-Item crates\dwmend\wix\License.rtf target\wix\License.rtf -Force
Copy-Item assets\icon.ico              target\wix\dwmend.ico   -Force

cargo wix -p dwmend --no-build --output target\wix\dwmend.msi
```

## Run

```pwsh
# Start as a background process
Start-Process .\target\release\dwmend.exe -WindowStyle Hidden

# Diagnostic: print what would be managed without touching anything
.\target\release\dwmend.exe dryrun

# Recover after a hard crash
.\target\release\dwmend.exe restore

# Launch on sign-in (per-user; no elevation; uninstall with `autostart disable`)
.\target\release\dwmend.exe autostart enable
.\target\release\dwmend.exe autostart status
.\target\release\dwmend.exe autostart disable

# Drive the running daemon from a IPC
.\target\release\dwmend.exe cmd "focus left"
.\target\release\dwmend.exe cmd "workspace 3"
.\target\release\dwmend.exe cmd "notify info 'Build complete'"
.\target\release\dwmend.exe query state
.\target\release\dwmend.exe query focused

# Help
.\target\release\dwmend.exe help
```

On first launch a default config is emitted to
`%APPDATA%\dwmend\config.toml`. Logs go to
`%LOCALAPPDATA%\dwmend\dwmend.log.<date>`.

## Default hotkeys

| Combo | Action |
|---|---|
| `Alt+H/J/K/L` | focus left/down/up/right |
| `Alt+Shift+H/J/K/L` | move focused tile (or translate float by 32 px) |
| `Alt+Ctrl+H/J/K/L` | resize 32 px (tiled split or floating edge) |
| `Alt+1`..`Alt+0` | switch to workspace 1..10 |
| `Alt+Shift+1`..`Alt+Shift+0` | move focused window to workspace |
| `Alt+T` / `Alt+F` / `Alt+W` | toggle float / monocle / close |
| `Alt+G` | smart toggle stack (merge with sibling, or 1-member fallback) |
| `Alt+Shift+G` | pop focused stack member back out into its own tile |
| `Alt+]` / `Alt+[` | cycle to next / previous stack member |
| `Alt+P` | pause/resume **DWMend** |
| `Alt+Shift+R` / `Alt+Shift+Q` | reload config / quit |
| `Alt+E` | open / dismiss the window-peek picker |
| `Alt+Enter` | confirm the peek selection (focus highlighted window) |
| Bar pill click | switch the clicked bar's monitor to that workspace |

The `toggle_layout` action (Dwindle ↔ Spiral on the focused workspace) ships unbound; add `"ALT+SHIFT+L" = "toggle_layout"` (or any other combo) to your `[keybindings]` to wire it up.

If any binding fails to register (because another app already claimed it),
**DWMend** logs a warning at startup and that binding is silently no-op.

> **Existing configs**: DWMend only writes the default config on first run. New options (the `[bar]` section, `[notifications]`, `[peek]`, `general.layout`, the stack / `toggle_layout` actions, the `workspace = N` rule action, the `notify` / `peek_toggle` / `peek_confirm` actions) won't be present in your existing `%APPDATA%\dwmend\config.toml` — add them manually. The defaults match the previous hard-coded values, so omitting them keeps the old behaviour. Keybinding edits ARE picked up by `Alt+Shift+R` (the listener thread is torn down and respawned with the new table).

## Privacy

Personal-use software. No installer, no signing, no telemetry.
The IPC pipe is local-only (no network access).
See `assets/config.toml` for the full set of options.

## Workspace layout

| Crate | Purpose |
| --- | --- |
| [`crates/dwmend-platform`](crates/dwmend-platform) | Thin `unsafe` Win32 wrappers (windows, monitors, hooks, DWM, DPI, focus border, IPC pipe transport) |
| [`crates/dwmend-layout`](crates/dwmend-layout)   | Pure-Rust BSP layout engine — no Windows deps, 18 unit tests |
| [`crates/dwmend`](crates/dwmend)          | Daemon library + thin binary: state, config, hotkeys, event loop, status bar, tray icon |
