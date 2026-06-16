<p align="center">
  <img src="assets/icon.png" alt="DWMend logo" width="128" height="128" />
</p>

# DWMend — Tiling Window Manager for Windows 11

A lightweight (and opinionated) tiling window manager for Windows 11, inspired by
[Hyprland](https://hyprland.org/) and [komorebi](https://github.com/LGUG2Z/komorebi). A mend for the DWM.

## Features

- **Lightweight** — single daemon binary.
- **No elevation, no installer** — run as a user without elevated Administration permissions. `dwmend autostart enable` writes an HKCU `Run` entry so the daemon launches on next sign-in. `dwmend autostart disable` removes it.
- **Dynamic tiling** — Binary Space Partition (BSP) approach ie. dwindle-style, one tree per monitor.
- **Global workspace pool** — 10 named workspaces; `Alt+#` (where # is the workspace number) brings a workspace to the focused monitor (swapping it from whichever monitor was showing it). `Alt+Shift+N` moves the focused window *and* follows focus to the target workspace.
- **Status bar** — thin GDI bar on each monitor showing workspace pills (active / visible-elsewhere / occupied / empty), the focused window title centred on the bar and a clock / battery / network indicators on the right.
- **Active tile focus border** — thick configurable-colour overlay frame around the active tile.
- **Stack containers** — group multiple windows into one tile. `Alt+G` is smart: with two tiles side-by-side it merges them into a stack on the first press; with a single tile it converts it to a 1-member stack so subsequent windows pile in. `Alt+[` / `Alt+]` cycle through. `Alt+Shift+G` pops the focused member back out into its own tile. The bar shows a `[2/3]` indicator next to the focused title when applicable.
- **TOML config + hot reload** — gaps, colours, rules update on save. 
  (Keybindings need a daemon restart due to `RegisterHotKey` semantics.)
- **IPC** — named-pipe server at `\\.\pipe\DwmendDaemon-v1`. Use `dwmend cmd "focus left"` from PowerShell / AutoHotKey / StreamDeck to drive the daemon, or `dwmend query state` to introspect.
- **Crash recovery** — `dwmend.exe restore` makes any windows **DWMend** hid visible again if the daemon crashes or is force-killed.

## Build

Requires:
- Rust stable on `x86_64-pc-windows-msvc`
- Visual Studio Build Tools or VS with the C++ workload + Windows 11 SDK
- A Developer PowerShell (`Launch-VsDevShell.ps1 -Arch amd64`)

```pwsh
cargo build --release
```

The release binary at `target\release\dwmend.exe` (~2.7 MB).

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
| `Alt+Shift+H/J/K/L` | move focused tile |
| `Alt+Ctrl+H/J/K/L` | resize 32 px |
| `Alt+1`..`Alt+0` | switch to workspace 1..10 |
| `Alt+Shift+1`..`Alt+Shift+0` | move focused window to workspace |
| `Alt+T` / `Alt+F` / `Alt+W` | toggle float / monocle / close |
| `Alt+G` | smart toggle stack (merge with sibling, or 1-member fallback) |
| `Alt+Shift+G` | pop focused stack member back out into its own tile |
| `Alt+]` / `Alt+[` | cycle to next / previous stack member |
| `Alt+P` | pause/resume **DWMend** |
| `Alt+Shift+R` / `Alt+Shift+Q` | reload config / quit |

If any binding fails to register (because another app already claimed it),
**DWMend** logs a warning at startup and that binding is silently no-op.

> **Existing configs**: DWMen only writes the default config on first run. Bindings introduced after your initial install (such as the stack commands above) won't be present in your existing `%APPDATA%\dwmend\config.toml` — add them manually and restart `dwmend.exe`. Keybinding changes are not picked up by `Alt+Shift+R` (a `RegisterHotKey` limitation).

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
