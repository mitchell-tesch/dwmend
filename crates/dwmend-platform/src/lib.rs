//! DWMend platform layer — all `unsafe` Win32 calls live here.
//!
//! Re-exports geometric types from `dwmend-layout` so downstream crates only
//! need this crate as a Win32 dependency.

pub mod defer_pos;
pub mod display_change;
pub mod dpi;
pub mod dwm;
pub mod enumerate;
pub mod focus_border;
pub mod focus_sink;
pub mod foreground;
pub mod ipc;
pub mod keyboard;
pub mod monitor;
pub mod rect;
pub mod window;
pub mod winevent;

pub use dwmend_layout::rect::{Axis, Direction, Rect};

/// Convenience alias used throughout the codebase.
pub type Result<T> = color_eyre::Result<T>;

/// Re-export the raw HWND type so callers don't need a windows-rs dependency
/// just to talk about window handles.
pub use windows::Win32::Foundation::HWND;
