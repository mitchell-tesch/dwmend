//! `ui` — host-process UI subsystems.
//!
//! These modules render and manage on-screen elements that are part of
//! the DWMend product surface: the per-monitor status [`bar`] and the
//! system [`tray`] icon. They live in the daemon crate (rather than the
//! generic `dwmend-platform`) because they own product-domain types
//! (`BarSnapshot::workspaces`, `TrayAction::TogglePause`) and would
//! pollute platform's "thin Win32 wrappers" mandate.

pub mod bar;
pub mod peek;
pub mod toast;
pub mod tray;
