//! DWMend — Windows 11 tiling window manager.
//!
//! Thin binary entry point. Everything lives in the `dwmend` library so
//! integration tests and future tooling can import individual pieces.

fn main() -> color_eyre::Result<()> {
    dwmend::run()
}
