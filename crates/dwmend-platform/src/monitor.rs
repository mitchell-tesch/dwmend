//! Monitor enumeration with stable identifiers.
//!
//! A `MonitorInfo::stable_id` survives display-mode changes, DPI scaling
//! changes, and (mostly) hot-plug — it is derived from the device path string
//! returned by `EnumDisplayDevicesW`, which encodes the PnP device ID.

use crate::rect::ToRect;
use crate::{Rect, Result};
use color_eyre::eyre::eyre;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::core::BOOL;
use windows::core::PCWSTR;

/// A single physical or virtual display, as DWMend models it.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Raw HMONITOR — *not* stable across display changes, but useful for
    /// short-lived operations (`MonitorFromWindow` lookups, etc.).
    pub hmonitor: isize,
    /// Stable, persistent identifier. Format: `"\\\\.\\DISPLAY1|<pnp-id>"`.
    /// Survives DPI/mode changes; survives plug/unplug if the same physical
    /// display returns to the same port.
    pub stable_id: String,
    /// Friendly name, e.g. `"DELL U2723QE"` — best-effort, may be empty.
    pub friendly_name: String,
    /// Full bounds (e.g. 3840×2160 for a 4K monitor).
    pub bounds: Rect,
    /// Work area = bounds minus taskbar / docked toolbars.
    pub work_area: Rect,
    /// DPI of this monitor (96 = 100%, 144 = 150%, 192 = 200%).
    pub dpi: u32,
    /// True if this is the primary display.
    pub primary: bool,
}

/// Enumerate all currently-attached monitors in OS order.
pub fn enumerate() -> Result<Vec<MonitorInfo>> {
    // A boxed Vec on the heap so the LPARAM remains valid for the entire callback chain.
    let mut sink: Box<Vec<MonitorInfo>> = Box::default();
    let lparam = LPARAM(std::ptr::from_mut(sink.as_mut()) as isize);

    // SAFETY: callback has matching extern signature; LPARAM points to a
    // valid Vec for the duration of the call.
    unsafe {
        EnumDisplayMonitors(None, None, Some(monitor_enum_proc), lparam)
            .ok()
            .map_err(|e| eyre!("EnumDisplayMonitors failed: {e}"))?;
    }

    Ok(*sink)
}

unsafe extern "system" fn monitor_enum_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: lparam was constructed in `enumerate` as a pointer to a
    // heap-allocated Vec<MonitorInfo> with the correct alignment & lifetime.
    let sink = unsafe { &mut *(lparam.0 as *mut Vec<MonitorInfo>) };

    match query_one(hmonitor) {
        Ok(info) => sink.push(info),
        Err(e) => tracing::warn!(?hmonitor, error = %e, "skipping monitor: query failed"),
    }
    BOOL(1) // continue enumeration
}

fn query_one(hmonitor: HMONITOR) -> Result<MonitorInfo> {
    // MONITORINFOEXW carries the szDevice GDI name we need for the stable ID.
    let mut info_ex = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };

    // SAFETY: info_ex is zeroed except cbSize; matches what the API expects.
    unsafe {
        GetMonitorInfoW(
            hmonitor,
            std::ptr::from_mut(&mut info_ex).cast::<MONITORINFO>(),
        )
        .ok()
        .map_err(|e| eyre!("GetMonitorInfoW failed: {e}"))?;
    }

    let bounds = info_ex.monitorInfo.rcMonitor.to_rect();
    let work_area = info_ex.monitorInfo.rcWork.to_rect();
    let primary = info_ex.monitorInfo.dwFlags & 1 != 0; // MONITORINFOF_PRIMARY

    let device_name = utf16_to_string(&info_ex.szDevice);

    // Get DPI for this monitor.
    let mut dpi_x: u32 = 96;
    let mut dpi_y: u32 = 96;
    // SAFETY: both pointers are valid mutable u32.
    let _ = unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };

    // Get the friendly name + PnP device ID from EnumDisplayDevicesW.
    let (friendly_name, pnp_id) = display_device_info(&device_name);

    // Stable ID: combine GDI device name (e.g. \\.\DISPLAY1) with PnP ID so a
    // remapped DISPLAY1 still distinguishes from a true new monitor.
    let stable_id = if pnp_id.is_empty() {
        device_name.clone()
    } else {
        format!("{device_name}|{pnp_id}")
    };

    Ok(MonitorInfo {
        hmonitor: hmonitor.0 as isize,
        stable_id,
        friendly_name,
        bounds,
        work_area,
        dpi: dpi_x,
        primary,
    })
}

/// `(friendly_name, pnp_id)` for the GDI device name, both best-effort.
fn display_device_info(device_name: &str) -> (String, String) {
    if device_name.is_empty() {
        return (String::new(), String::new());
    }
    // Convert device_name → wide string (null-terminated).
    let mut wide: Vec<u16> = device_name.encode_utf16().collect();
    wide.push(0);
    let device_pcwstr = PCWSTR(wide.as_ptr());

    // First call: adapter (index 0) — friendly name lives in DeviceString.
    let mut adapter = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };

    let mut friendly = String::new();
    // SAFETY: device_pcwstr is null-terminated; adapter has cb set.
    if unsafe { EnumDisplayDevicesW(device_pcwstr, 0, &mut adapter, 0) }.as_bool() {
        friendly = utf16_to_string(&adapter.DeviceString);
    }

    // Now query monitor child (also index 0). Its DeviceID is the PnP path.
    let mut monitor = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    let mut pnp = String::new();
    // SAFETY: same constraints.
    if unsafe { EnumDisplayDevicesW(device_pcwstr, 0, &mut monitor, 1) }.as_bool() {
        pnp = utf16_to_string(&monitor.DeviceID);
        // If we have a monitor child name, prefer it as friendly.
        let mon_friendly = utf16_to_string(&monitor.DeviceString);
        if !mon_friendly.is_empty() {
            friendly = mon_friendly;
        }
    }

    (friendly, pnp)
}

/// Trim a fixed-size wide buffer at the first NUL and convert lossily.
fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
