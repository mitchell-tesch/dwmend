//! Status bar — a thin always-on-top window per monitor that displays
//! workspace indicators and the focused window title.
//!
//! ## Why built-in
//!
//! Keeps it lightweight and simple. No separate process no WebView
//! dependency tree, and the same WM_PAINT model we already use for the
//! focus border overlay.
//!
//! ## Architecture
//!
//! * One dedicated thread owns N bar windows (one per monitor) and runs a
//!   `GetMessage` pump for all of them.
//! * Each bar window stores a per-monitor `BarSnapshot` in a `Mutex` and
//!   posts a `WM_USER + 1` message when the host updates it; the pump
//!   handler responds with `InvalidateRect` to force a redraw on the next
//!   `WM_PAINT`.
//! * Rendering is plain GDI: solid-color background, `DrawTextW` for the
//!   workspace numbers and focused title. No animation, no buttons (yet).
//! * The bar is `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST` so
//!   it never appears in Alt-Tab, never steals focus, and floats above
//!   normal windows. It is *not* click-through — `WS_EX_TRANSPARENT` would
//!   let clicks fall through to whatever's behind it, but the bar wants to
//!   eat clicks (and later use them for workspace switching).
//!
//! ## State sharing
//!
//! The host crate calls [`update`] whenever the focused window, focused
//! monitor, or workspace contents change. The function updates every bar's
//! snapshot, then posts a single message per bar so the pump thread handles
//! redraws on its own time — the host never blocks on GDI.

use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Sender, bounded};
use dwmend_platform::Rect;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION,
    FW_NORMAL, FillRect, GetStockObject, HDC, HGDIOBJ, InvalidateRect, NULL_BRUSH, PAINTSTRUCT,
    PROOF_QUALITY, PS_SOLID, Rectangle, SRCCOPY, SelectObject, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_FRIENDLY_NAME,
    GAA_FLAG_SKIP_MULTICAST, GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses,
    IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DI_NORMAL, DispatchMessageW, DrawIconEx,
    GetMessageW, HICON, HWND_TOPMOST, IDC_ARROW, IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED,
    LoadCursorW, LoadImageW, MSG, PostMessageW, PostThreadMessageW, RegisterClassExW,
    SET_WINDOW_POS_FLAGS, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetTimer,
    SetWindowPos, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_DESTROY, WM_ERASEBKGND,
    WM_PAINT, WM_QUIT, WM_TIMER, WM_USER, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

/// Default bar height in pixels.
pub const DEFAULT_HEIGHT: i32 = 28;

/// Resource ID of the application icon embedded by `crates/dwmend/build.rs`
/// (`1 ICON "..\\..\\assets\\icon.ico"`). The bar loads this at the chosen
/// pixel size and renders it on the left edge in front of the workspace
/// pills. Tray.rs uses the same id for the system-tray icon.
const IDI_DWMEND_APP: u16 = 1;

/// Message we post to a bar window when its snapshot has been updated.
const WM_WTM_BAR_REFRESH: u32 = WM_USER + 1;

/// Thread message posted to the bar listener thread to request a monitor
/// topology sync. Bar HWNDs whose `monitor_id` is no longer in the
/// pending-specs list are destroyed; new HWNDs are created for every spec
/// without a current handle. The bar thread is the only place where the
/// listener `GetMessageW` lives, so we have to do this work there — not on
/// the host thread — to keep WM_PAINT routing intact.
const WM_WTM_BAR_SYNC: u32 = WM_USER + 2;

/// Timer id used by every bar window to refresh the live clock once per
/// second. The timer is delivered as a `WM_TIMER` message; we re-format the
/// clock and invalidate the bar so the next `WM_PAINT` picks it up.
/// (The minute label updates implicitly; the cost of one full repaint per
/// second is microseconds of GDI and is amortised by DWM compositing.)
const TIMER_ID_CLOCK: usize = 1;

/// Per-monitor render state that the host writes and the bar thread reads.
///
/// `PartialEq` is intentional: `update` compares against the current stored
/// snapshot and skips the `PostMessage` + repaint when nothing changed.
/// Without this, fast-firing WinEvents (mouse hover, animations) produce a
/// flood of identical snapshots and the bar thread paints uselessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarSnapshot {
    pub workspaces: Vec<WorkspaceState>,
    pub focused_title: String,
    /// Optional indicator text shown right-aligned (e.g. " PAUSED ").
    pub right_label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceState {
    pub id: u32,
    /// This is the workspace currently shown on the bar's monitor.
    pub is_active: bool,
    /// This workspace is visible on *some* monitor (could be this one).
    pub is_visible: bool,
    /// Workspace contains at least one managed window (helps users see
    /// which workspaces are "live" without switching to each one).
    pub has_windows: bool,
}

/// A spec describing where a single bar lives.
#[derive(Debug, Clone)]
pub struct BarSpec {
    /// Stable monitor id — the host uses this to address `update` calls.
    pub monitor_id: String,
    /// Full screen bounds of the host monitor (in pixels).
    pub bounds: Rect,
}

/// Colors used by the bar. Background/border are GDI `COLORREF` triples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarColors {
    pub background: u32,
    pub foreground: u32,
    pub active_bg: u32,
    pub active_fg: u32,
    pub visible_outline: u32,
    pub dim_fg: u32,
}

impl Default for BarColors {
    fn default() -> Self {
        Self {
            background: rgb(0x1E, 0x1E, 0x2E),
            foreground: rgb(0xC0, 0xC0, 0xC0),
            active_bg: rgb(0x4F, 0xC3, 0xF7),
            active_fg: rgb(0x10, 0x10, 0x18),
            visible_outline: rgb(0x80, 0x80, 0x80),
            dim_fg: rgb(0x60, 0x60, 0x60),
        }
    }
}

// ---- shared state ----------------------------------------------------------

/// One slot per monitor; lookup keyed by stable monitor id.
type BarMap = HashMap<String, BarHandle>;

struct BarHandle {
    /// HWND as `isize` so it's Send.
    hwnd: isize,
    snapshot: Arc<Mutex<BarSnapshot>>,
}

static BARS: OnceLock<Mutex<BarMap>> = OnceLock::new();
static COLORS: OnceLock<Mutex<BarColors>> = OnceLock::new();
static HEIGHT: AtomicIsize = AtomicIsize::new(DEFAULT_HEIGHT as isize);
static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);
/// Pending spec list for the next WM_WTM_BAR_SYNC. The host writes,
/// the bar thread consumes.
static PENDING_SYNC: Mutex<Option<Vec<BarSpec>>> = Mutex::new(None);

/// Cached GDI resources used by every `draw_bar` call. Without this cache,
/// each paint creates and destroys a font + 2 brushes + 1 pen — `CreateFontW`
/// alone runs the font-fallback selector on every invocation. With one bar
/// paint per second from the clock tick plus extra paints on every event,
/// that's significant pointless GDI churn.
///
/// The cache is invalidated automatically when `(height, colors)` differ
/// from what produced the cached handles; on miss the old handles are
/// `DeleteObject`ed and fresh ones installed.
///
/// Resources are `isize` because GDI handles are `*mut c_void` (not `Send`)
/// but the values are pointer-sized integers we cast back at the use site.
struct BarResources {
    font: isize,         // HFONT
    bg_brush: isize,     // HBRUSH for bar background
    active_brush: isize, // HBRUSH for the active workspace pill fill
    visible_pen: isize,  // HPEN for the "visible on other monitor" outline
    key_height: i32,
    key_colors: BarColors,
}

static RESOURCES: Mutex<Option<BarResources>> = Mutex::new(None);

/// Snapshot of the cached resource handles returned to `draw_bar` so it can
/// release the global cache lock before doing any GDI work. The handles
/// themselves remain owned by the cache; `draw_bar` must NOT `DeleteObject`
/// them.
#[derive(Clone, Copy)]
struct CachedHandles {
    font: isize,
    bg_brush: isize,
    active_brush: isize,
    visible_pen: isize,
}

/// Return cached GDI handles for `(height, colors)`, creating them lazily.
/// On a key mismatch the previous handles are freed before reallocation —
/// the cache is single-slot so a config-reload that changes colors flushes
/// the entire cache.
fn get_or_create_resources(height: i32, colors: &BarColors) -> CachedHandles {
    let mut guard = RESOURCES.lock();
    if let Some(r) = guard.as_ref()
        && r.key_height == height
        && r.key_colors == *colors
    {
        return CachedHandles {
            font: r.font,
            bg_brush: r.bg_brush,
            active_brush: r.active_brush,
            visible_pen: r.visible_pen,
        };
    }

    // Free the stale entry, if any. Safe: handles came from this same
    // CreateFontW / CreateSolidBrush / CreatePen path on a previous call.
    if let Some(r) = guard.take() {
        // SAFETY: all handles were produced by the GDI creator functions
        // below; DeleteObject is idempotent on a null pointer.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(r.font as *mut _));
            let _ = DeleteObject(HGDIOBJ(r.bg_brush as *mut _));
            let _ = DeleteObject(HGDIOBJ(r.active_brush as *mut _));
            let _ = DeleteObject(HGDIOBJ(r.visible_pen as *mut _));
        }
    }

    let mut face = utf16(b"Segoe UI Variable\0");
    // SAFETY: CreateFontW is a pure GDI allocator; null return is handled
    // implicitly because every consumer treats a null handle as a no-op.
    let font = unsafe {
        CreateFontW(
            (height - 10).max(12), // height
            0,                     // width (0 = auto)
            0,                     // escapement
            0,                     // orientation
            FW_NORMAL.0 as i32,    // weight (FW_NORMAL = 400)
            0,                     // italic
            0,                     // underline
            0,                     // strikeout
            // windows-rs 0.62 made these typed newtypes; passing 0 means
            // "system default" for charset / out-precision / clip-precision
            // exactly as before.
            windows::Win32::Graphics::Gdi::FONT_CHARSET(0),
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            PROOF_QUALITY, // quality (already a typed FONT_QUALITY)
            0,             // pitch & family
            PCWSTR(face.as_mut_ptr()),
        )
    };
    // SAFETY: CreateSolidBrush and CreatePen are pure GDI allocators.
    let bg_brush = unsafe { CreateSolidBrush(COLORREF(colors.background)) };
    let active_brush = unsafe { CreateSolidBrush(COLORREF(colors.active_bg)) };
    let visible_pen = unsafe { CreatePen(PS_SOLID, 1, COLORREF(colors.visible_outline)) };

    let handles = CachedHandles {
        font: font.0 as isize,
        bg_brush: bg_brush.0 as isize,
        active_brush: active_brush.0 as isize,
        visible_pen: visible_pen.0 as isize,
    };
    *guard = Some(BarResources {
        font: handles.font,
        bg_brush: handles.bg_brush,
        active_brush: handles.active_brush,
        visible_pen: handles.visible_pen,
        key_height: height,
        key_colors: *colors,
    });
    handles
}

/// Drop every cached GDI handle. Called from `stop` so the daemon doesn't
/// leak font/brush handles into a subsequent run (Windows reclaims on
/// process exit anyway, but explicit cleanup makes Application Verifier
/// happy and keeps the test harness leak-free).
fn drop_cached_resources() {
    let mut guard = RESOURCES.lock();
    if let Some(r) = guard.take() {
        // SAFETY: handles were produced by GDI creators above.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(r.font as *mut _));
            let _ = DeleteObject(HGDIOBJ(r.bg_brush as *mut _));
            let _ = DeleteObject(HGDIOBJ(r.active_brush as *mut _));
            let _ = DeleteObject(HGDIOBJ(r.visible_pen as *mut _));
        }
    }
}

/// Cached `HICON` for the app icon shown on the left of every bar.
///
/// The bar height is fixed for a session (set by `bar::start` and never
/// reassigned), so a single sized icon serves every paint. We load it lazily
/// on the first paint with `LR_SHARED`, which means Windows owns the
/// lifetime — we must NOT call `DestroyIcon` on it. The handle is stored as
/// `isize` so this static is `Send + Sync`; the cast back to `HICON` happens
/// at the call site.
///
/// `0` means "load failed" (very unlikely — the resource is embedded by
/// `build.rs`); the caller falls back to the original icon-less layout so
/// the bar still works.
static BAR_ICON: OnceLock<isize> = OnceLock::new();

/// Return the bar icon at the given pixel size, loading on first call.
/// The size is captured on the first call and reused for the whole session;
/// subsequent calls with a different size are ignored to keep the cache
/// single-slot, matching the fact that `HEIGHT` doesn't change after
/// `bar::start`.
fn bar_icon(size_px: i32) -> Option<HICON> {
    let raw = *BAR_ICON.get_or_init(|| {
        // SAFETY: GetModuleHandleW(None) returns the running EXE's HMODULE.
        let Ok(hmod) = (unsafe { GetModuleHandleW(None) }) else {
            return 0;
        };
        let hinst = HINSTANCE(hmod.0);
        // MAKEINTRESOURCE for a numeric resource id is the integer cast
        // into a wstr pointer — anything in the low 64K is treated as an
        // ordinal rather than a name lookup.
        let name = PCWSTR(IDI_DWMEND_APP as usize as *const u16);
        // SAFETY: hinst is valid; name is a numeric MAKEINTRESOURCE; size
        // params are positive. `LR_SHARED` means "do not free me" — the
        // OS keeps a process-wide cache and returns the same HICON for
        // matching calls.
        let h = unsafe {
            LoadImageW(
                Some(hinst),
                name,
                IMAGE_ICON,
                size_px,
                size_px,
                LR_DEFAULTCOLOR | LR_SHARED,
            )
        };
        match h {
            Ok(handle) if !handle.is_invalid() => handle.0 as isize,
            _ => 0,
        }
    });
    if raw == 0 {
        None
    } else {
        Some(HICON(raw as *mut _))
    }
}

// ---- public API ------------------------------------------------------------

/// Initialise the bar subsystem with one bar per `spec`. Subsequent calls to
/// [`refresh_monitors`] are how the host responds to display hot-plug events.
pub fn start(specs: Vec<BarSpec>, height: i32, colors: BarColors) -> Result<()> {
    HEIGHT.store(height.max(16) as isize, Ordering::SeqCst);
    let _ = BARS.set(Mutex::new(BarMap::new()));
    let _ = COLORS.set(Mutex::new(colors));

    let (init_tx, init_rx) = bounded::<std::result::Result<(), String>>(1);

    std::thread::Builder::new()
        .name("dwmend-bar".into())
        .spawn(move || run_bar_thread(specs, init_tx))
        .map_err(|e| eyre!("spawn dwmend-bar thread: {e}"))?;

    match init_rx
        .recv()
        .map_err(|_| eyre!("bar thread died during init"))?
    {
        Ok(()) => {
            tracing::info!(height, "status bar subsystem initialised");
            Ok(())
        }
        Err(e) => Err(eyre!("bar init failed: {e}")),
    }
}

/// Push a fresh snapshot to the bar bound to `monitor_id`. No-op if no such
/// bar exists (e.g. the monitor was hot-removed before this update fired).
///
/// Snapshots are deduplicated against the bar's current state: if the new
/// snapshot is byte-for-byte identical to what's already displayed, the
/// `PostMessage` (and the subsequent `WM_PAINT`) are skipped. This is the
/// second line of defense against runaway repaints — the first being
/// `events::handle` returning `false` for `LocationChanged` and similar.
pub fn update(monitor_id: &str, snapshot: BarSnapshot) {
    let Some(bars) = BARS.get() else { return };
    let bars = bars.lock();
    let Some(handle) = bars.get(monitor_id) else {
        return;
    };
    {
        let mut current = handle.snapshot.lock();
        if *current == snapshot {
            return; // identical → no repaint needed
        }
        *current = snapshot;
    }
    // Wake the bar thread; the WM_PAINT it triggers will read the snapshot.
    // SAFETY: hwnd is one of our own windows; posting a custom message is safe.
    unsafe {
        let _ = PostMessageW(
            Some(HWND(handle.hwnd as *mut _)),
            WM_WTM_BAR_REFRESH,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Cooperative shutdown — destroy every bar window and end the thread.
pub fn stop() {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    // SAFETY: posting to a thread ID is always safe.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

/// Reconcile the live bar set with `specs`. Any bar whose `monitor_id` is
/// not in `specs` has its HWND destroyed (so dead-monitor bars stop
/// floating around on the surviving display); any spec without a current
/// HWND gets a fresh one created at its bounds.
///
/// Posts a thread message to the bar listener; safe to call from any
/// thread. No-op if the bar subsystem hasn't been started.
pub fn sync_monitors(specs: Vec<BarSpec>) {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    *PENDING_SYNC.lock() = Some(specs);
    // SAFETY: posting to a thread ID is always safe.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_WTM_BAR_SYNC, WPARAM(0), LPARAM(0));
    }
}

// ---- bar thread ------------------------------------------------------------

fn run_bar_thread(specs: Vec<BarSpec>, init_tx: Sender<std::result::Result<(), String>>) {
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    let class_name = utf16(b"DwmendStatusBar\0");

    // SAFETY: GetModuleHandleW(None) returns the current EXE.
    let hinst = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => {
            let _ = init_tx.send(Err(format!("GetModuleHandleW: {e}")));
            return;
        }
    };

    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        // Without an explicit hCursor the OS falls back to IDC_APPSTARTING
        // (the busy/spinner cursor) whenever the pointer hovers over the
        // bar. Load the standard arrow so the cursor stays normal.
        // SAFETY: LoadCursorW with HINSTANCE=NULL and IDC_ARROW returns a
        // shared system cursor handle owned by the OS — we never free it.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };
    // SAFETY: class fully initialised; class_name is null-terminated wstr.
    let class_atom = unsafe { RegisterClassExW(&class) };
    if class_atom == 0 {
        let _ = init_tx.send(Err("RegisterClassExW failed".into()));
        return;
    }

    let height = HEIGHT.load(Ordering::SeqCst) as i32;
    let class_name_ptr = class_name.as_ptr();
    if let Some(bars) = BARS.get() {
        let mut bars = bars.lock();
        for spec in &specs {
            create_bar_hwnd(&mut bars, spec, hinst, class_name_ptr, height);
        }
    }

    let _ = init_tx.send(Ok(()));
    tracing::info!(count = specs.len(), "status bar thread started");

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param; hwnd=None pumps all our bars.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        // Thread messages (msg.hwnd is null) we need to handle inline \u2014
        // DispatchMessageW skips them.
        if msg.hwnd.0.is_null() && msg.message == WM_WTM_BAR_SYNC {
            if let Some(specs) = PENDING_SYNC.lock().take() {
                sync_bars_on_thread(&specs, hinst, class_name_ptr, height);
            }
            continue;
        }
        // SAFETY: msg is populated.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup: destroy every bar window.
    if let Some(bars) = BARS.get() {
        let bars = bars.lock();
        for handle in bars.values() {
            // SAFETY: HWND from our own CreateWindow.
            let _ = unsafe { DestroyWindow(HWND(handle.hwnd as *mut _)) };
        }
    }
    // Free the cached GDI font / brushes / pen so the daemon doesn't leak
    // them across a soft restart inside the same process (relevant for
    // tests and any future hot-restart path; on normal process exit the
    // OS reclaims everything anyway).
    drop_cached_resources();
    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("status bar thread exited");
}

/// Create a bar HWND for `spec` and insert a `BarHandle` into `bars`. Used
/// by both the initial startup pass and `sync_bars_on_thread`. Must run on
/// the bar listener thread so the new HWND's WM_PAINT routes back here.
fn create_bar_hwnd(
    bars: &mut BarMap,
    spec: &BarSpec,
    hinst: HINSTANCE,
    class_name_ptr: *const u16,
    height: i32,
) {
    let ex = WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0);
    let class_name = PCWSTR(class_name_ptr);
    // SAFETY: class_name pointer outlives the call (kept alive on the bar
    // thread's stack); other parameters are constants or just-validated.
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            class_name,
            class_name,
            WS_POPUP,
            spec.bounds.x,
            spec.bounds.y,
            spec.bounds.w,
            height,
            None, // hWndParent — unparented top-level
            None, // hMenu — none
            Some(hinst),
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, monitor = %spec.monitor_id, "bar window creation failed");
            return;
        }
    };
    let snapshot = Arc::new(Mutex::new(BarSnapshot {
        workspaces: Vec::new(),
        focused_title: String::new(),
        right_label: None,
    }));
    bars.insert(
        spec.monitor_id.clone(),
        BarHandle {
            hwnd: hwnd.0 as isize,
            snapshot,
        },
    );
    // SAFETY: HWND from our own CreateWindow.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    // 1 Hz clock tick; auto-killed by DestroyWindow.
    // SAFETY: hwnd valid; TIMERPROC=None routes to WM_TIMER.
    let _ = unsafe { SetTimer(Some(hwnd), TIMER_ID_CLOCK, 1000, None) };
}

/// Reconcile the live BarMap with `specs`. Destroys any HWND whose
/// `monitor_id` is no longer in `specs`, then creates fresh HWNDs for any
/// spec without a current handle. Must run on the bar listener thread.
fn sync_bars_on_thread(
    specs: &[BarSpec],
    hinst: HINSTANCE,
    class_name_ptr: *const u16,
    height: i32,
) {
    let Some(bars) = BARS.get() else { return };
    let mut bars = bars.lock();
    let wanted: std::collections::HashSet<&str> =
        specs.iter().map(|s| s.monitor_id.as_str()).collect();

    // 1. Destroy bars for monitors no longer present.
    let stale: Vec<String> = bars
        .keys()
        .filter(|k| !wanted.contains(k.as_str()))
        .cloned()
        .collect();
    for mid in stale {
        if let Some(handle) = bars.remove(&mid) {
            // SAFETY: HWND from our own CreateWindow.
            let _ = unsafe { DestroyWindow(HWND(handle.hwnd as *mut _)) };
            tracing::info!(monitor = %mid, "bar HWND destroyed (monitor removed)");
        }
    }

    // 2. Create bars for new monitors AND reposition surviving bars.
    //    The reposition pass matters because Windows can shift a
    //    surviving monitor's bounds after an unplug (e.g. a secondary
    //    monitor at (3440, 624) often relocates to (0, 0) once it is the
    //    only display). Without this, the bar HWND stays pinned at the
    //    pre-unplug coordinates and the user sees it "disappear" because
    //    it's now off-screen.
    for spec in specs {
        if let Some(handle) = bars.get(&spec.monitor_id) {
            let hwnd = HWND(handle.hwnd as *mut _);
            // SWP_SHOWWINDOW is defensive: if the OS hid the bar during
            // the topology change (it sometimes does for off-screen
            // top-most windows), show it again. SWP_NOACTIVATE preserves
            // focus. HWND_TOPMOST keeps the bar above app windows even
            // if the OS dropped its topmost flag during the transition.
            // SAFETY: hwnd is from our own CreateWindow; the SET_WINDOW_POS_FLAGS
            // bitset is constructed from documented constants.
            let r = unsafe {
                SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    spec.bounds.x,
                    spec.bounds.y,
                    spec.bounds.w,
                    height,
                    SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0 | SWP_SHOWWINDOW.0),
                )
            };
            if let Err(e) = r {
                tracing::warn!(monitor = %spec.monitor_id, error = %e,
                    "bar reposition failed after topology change");
            }
        } else {
            create_bar_hwnd(&mut bars, spec, hinst, class_name_ptr, height);
            tracing::info!(monitor = %spec.monitor_id, "bar HWND created (monitor added)");
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_WTM_BAR_REFRESH => {
            // bErase=FALSE — our WM_PAINT handler fills every pixel via the
            // double-buffer, so we don't want the OS to wipe the bar first
            // (which is what produces flicker between snapshots).
            // SAFETY: HWND from our own CreateWindow.
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            // 1 Hz clock tick (TIMER_ID_CLOCK is the only timer on this window).
            // Same reasoning as WM_WTM_BAR_REFRESH — no OS-side erase.
            // SAFETY: HWND from our own CreateWindow.
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // Tell the OS we've handled background erasing. Returning a
            // non-zero LRESULT suppresses the default WM_ERASEBKGND that
            // would otherwise fill the client area with the (unset) class
            // brush — the exact flicker the user reported.
            LRESULT(1)
        }
        WM_PAINT => {
            // SAFETY: hwnd is valid; PAINTSTRUCT is a valid out-param.
            unsafe { handle_paint(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        // SAFETY: DefWindowProcW is the documented fallback handler.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn handle_paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: hwnd valid; ps is a valid out-param.
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if hdc.is_invalid() {
        return;
    }

    // Read snapshot for this bar.
    let snapshot = lookup_snapshot(hwnd);
    let colors = COLORS.get().map(|m| *m.lock()).unwrap_or_default();

    // Double-buffer: render to an off-screen memory DC, then BitBlt the
    // entire result onto the window DC. Without this, the user sees the
    // background fill on the screen DC for one frame before the text /
    // pills come in on top — visible as a 1 Hz flash at the clock tick.
    let width = ps.rcPaint.right - ps.rcPaint.left;
    let height = ps.rcPaint.bottom - ps.rcPaint.top;
    if width > 0 && height > 0 {
        // SAFETY: hdc is valid (from BeginPaint).
        let mem_dc = unsafe { CreateCompatibleDC(Some(hdc)) };
        let mem_bmp = unsafe { CreateCompatibleBitmap(hdc, width, height) };
        if !mem_dc.is_invalid() && !mem_bmp.is_invalid() {
            // SAFETY: mem_dc and mem_bmp are valid; old object is the default
            // 1x1 monochrome bitmap that always exists on a memory DC.
            let old_bmp = unsafe { SelectObject(mem_dc, HGDIOBJ(mem_bmp.0)) };
            // Memory DC's origin is (0,0); use a rect of the same size.
            let mem_rc = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            // SAFETY: mem_dc valid; geometry derived from the paint clip.
            unsafe { draw_bar(mem_dc, mem_rc, &snapshot, &colors) };
            // Atomically copy the finished bar onto the screen.
            // SAFETY: both DCs valid; src/dst rects match.
            let _ = unsafe {
                BitBlt(
                    hdc,
                    ps.rcPaint.left,
                    ps.rcPaint.top,
                    width,
                    height,
                    Some(mem_dc),
                    0,
                    0,
                    SRCCOPY,
                )
            };
            // SAFETY: restore default bitmap before destroying the DC; safe
            // even if old_bmp is null (SelectObject just no-ops).
            unsafe {
                SelectObject(mem_dc, old_bmp);
                let _ = DeleteObject(HGDIOBJ(mem_bmp.0));
                let _ = DeleteDC(mem_dc);
            }
        } else {
            // Couldn't allocate the memory DC — fall back to painting
            // directly. Loses double-buffering for this frame but still draws.
            // SAFETY: hdc valid; geometry comes from the PAINTSTRUCT clip.
            unsafe { draw_bar(hdc, ps.rcPaint, &snapshot, &colors) };
            // Clean up whichever handle was created.
            if !mem_dc.is_invalid() {
                // SAFETY: mem_dc was created by CreateCompatibleDC.
                let _ = unsafe { DeleteDC(mem_dc) };
            }
            if !mem_bmp.is_invalid() {
                // SAFETY: mem_bmp was created by CreateCompatibleBitmap.
                let _ = unsafe { DeleteObject(HGDIOBJ(mem_bmp.0)) };
            }
        }
    }

    // SAFETY: hwnd valid; ps is the same struct from BeginPaint.
    let _ = unsafe { EndPaint(hwnd, &ps) };
}

fn lookup_snapshot(hwnd: HWND) -> BarSnapshot {
    let key = hwnd.0 as isize;
    let Some(bars) = BARS.get() else {
        return empty_snapshot();
    };
    let bars = bars.lock();
    for handle in bars.values() {
        if handle.hwnd == key {
            return handle.snapshot.lock().clone();
        }
    }
    empty_snapshot()
}

fn empty_snapshot() -> BarSnapshot {
    BarSnapshot {
        workspaces: Vec::new(),
        focused_title: String::new(),
        right_label: None,
    }
}

unsafe fn draw_bar(hdc: HDC, rc: RECT, snap: &BarSnapshot, colors: &BarColors) {
    // Resolve cached GDI handles for the current (height, colors). On the
    // first paint after startup (or after a config reload that changed
    // colors) this allocates; thereafter it's a pure HashMap-style lookup.
    let bar_height = HEIGHT.load(Ordering::SeqCst) as i32;
    let res = get_or_create_resources(bar_height, colors);

    // Background fill — reuses the cached brush. FillRect does not consume
    // the brush handle, so the next paint reuses the same one.
    // SAFETY: bg_brush came from CreateSolidBrush and is alive until the
    // cache is invalidated or `drop_cached_resources` runs.
    let _ = unsafe {
        FillRect(
            hdc,
            &rc,
            windows::Win32::Graphics::Gdi::HBRUSH(res.bg_brush as *mut _),
        )
    };

    // Select the cached font for all text draws. Old font is restored at
    // the end of this function; we deliberately do NOT DeleteObject the
    // cached font — the cache owns it.
    // SAFETY: font handle is alive (see above).
    let old_font = unsafe { SelectObject(hdc, HGDIOBJ(res.font as *mut _)) };
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
    }

    // ---- app icon (very left) ----
    // Sits flush left with a small inset, vertically centred. Icon size
    // tracks the bar height so a taller bar gets a proportionally larger
    // logo. We load (with `LR_SHARED`) on the first paint and cache the
    // handle for the rest of the session.
    let icon_pad_left = 6;
    let icon_pad_right = 6;
    let bar_inner_h = rc.bottom - rc.top;
    let icon_size = (bar_inner_h - 8).clamp(16, 32);
    let icon_zone_left = rc.left + icon_pad_left;
    let icon_zone_right = if let Some(hicon) = bar_icon(icon_size) {
        let icon_y = rc.top + (bar_inner_h - icon_size) / 2;
        // SAFETY: hicon is a valid HICON (or LR_SHARED would have failed
        // and `bar_icon` returned None); hdc is the live paint DC; the
        // last two args are documented as "no flicker-free brush" / DI_NORMAL.
        let _ = unsafe {
            DrawIconEx(
                hdc,
                icon_zone_left,
                icon_y,
                hicon,
                icon_size,
                icon_size,
                0,
                None,
                DI_NORMAL,
            )
        };
        icon_zone_left + icon_size + icon_pad_right
    } else {
        // Fall back to the pre-icon layout so the bar still works if the
        // resource is missing for any reason (e.g. EXE was stripped).
        rc.left
    };

    // ---- workspace pills (left side) ----
    // Pill spans the full bar height — no top/bottom inset — with a small
    // horizontal padding so adjacent pills don't touch.
    let pill_w = (rc.bottom - rc.top) - 6;
    let mut x = icon_zone_right;
    for ws in &snap.workspaces {
        let pill = RECT {
            left: x,
            top: rc.top,
            right: x + pill_w,
            bottom: rc.bottom,
        };
        // Decide background treatment + text colour for this pill.
        let text_color = if ws.is_active {
            // SAFETY: active_brush owned by cache.
            let _ = unsafe {
                FillRect(
                    hdc,
                    &pill,
                    windows::Win32::Graphics::Gdi::HBRUSH(res.active_brush as *mut _),
                )
            };
            colors.active_fg
        } else if ws.is_visible {
            // Outline the workspace shown on another monitor. Stock
            // NULL_BRUSH keeps the interior transparent so the bar shows
            // through. Pen is cached; null brush is a stock object we
            // never own.
            // SAFETY: visible_pen owned by cache.
            let old_pen = unsafe { SelectObject(hdc, HGDIOBJ(res.visible_pen as *mut _)) };
            let null_brush = unsafe { GetStockObject(NULL_BRUSH) };
            let old_brush = unsafe { SelectObject(hdc, null_brush) };
            let _ = unsafe { Rectangle(hdc, pill.left, pill.top, pill.right, pill.bottom) };
            unsafe { SelectObject(hdc, old_brush) };
            unsafe { SelectObject(hdc, old_pen) };
            colors.foreground
        } else if ws.has_windows {
            colors.foreground
        } else {
            colors.dim_fg
        };

        let label = format!("{}", ws.id);
        // SAFETY: hdc valid and font is selected for the whole draw_bar call.
        unsafe { draw_centered_text(hdc, pill, &label, text_color, DRAW_TEXT_FORMAT(0)) };

        x += pill_w + 4;
    }
    let pill_zone_right = x;

    // ---- right edge: live clock + optional indicator ----
    // SAFETY: GetLocalTime takes no inputs and always succeeds.
    let now = unsafe { GetLocalTime() };
    // 12-hour clock with AM/PM. wHour is 0..=23 from GetLocalTime; convert
    // to 12-hour form: 0 -> 12 AM, 1..=11 -> AM, 12 -> 12 PM, 13..=23 -> PM.
    let (h12, suffix) = match now.wHour {
        0 => (12, "AM"),
        1..=11 => (now.wHour, "AM"),
        12 => (12, "PM"),
        _ => (now.wHour - 12, "PM"),
    };
    let clock_str = format!("{}:{:02} {}", h12, now.wMinute, suffix);
    let clock_w = measure_text(hdc, &clock_str);
    let clock_right = rc.right - 10;
    let clock_left = clock_right - clock_w;

    // ---- battery (drawn to the left of the clock) ----
    // The battery zone occupies its own slot on the right side; if the
    // device has no battery the slot collapses and the next zone (network /
    // right indicator) abuts the clock directly.
    let battery_str = current_battery().map(format_battery);
    let (battery_zone_left, battery_zone_right) = if let Some(ref s) = battery_str {
        let w = measure_text(hdc, s);
        let right = clock_left - 12;
        let left = right - w;
        (Some(left), Some(right))
    } else {
        (None, None)
    };

    // ---- network indicator (drawn to the left of the battery) ----
    // Same collapsing behaviour: when there's no active adapter (everything
    // offline) the slot vanishes and the right indicator sits where the
    // network glyph would have been.
    let network_anchor_right = battery_zone_left.unwrap_or(clock_left);
    let network_str = current_network_cached().map(format_network);
    let (network_zone_left, network_zone_right) = if let Some(ref s) = network_str {
        let w = measure_text(hdc, s);
        let right = network_anchor_right - 10;
        let left = right - w;
        (Some(left), Some(right))
    } else {
        (None, None)
    };

    // Right indicator (e.g. "PAUSED") sits to the LEFT of the network slot
    // (or battery, or clock) so the clock keeps a stable position.
    let right_anchor_left = network_zone_left
        .or(battery_zone_left)
        .unwrap_or(clock_left);
    let mut right_zone_left = right_anchor_left;
    let label_box = snap.right_label.as_ref().map(|label| {
        let w = measure_text(hdc, label);
        let right = right_anchor_left - 10;
        let left = right - w;
        right_zone_left = left;
        (left, right)
    });

    // ---- focused title (centred between pills and right zone) ----
    let safe_left = pill_zone_right + 10;
    let safe_right = right_zone_left - 10;
    let safe_w = safe_right - safe_left;
    if safe_w > 0 && !snap.focused_title.is_empty() {
        let title_w = measure_text(hdc, &snap.focused_title);
        let (tl, tr) = if title_w >= safe_w {
            // Doesn't fit — fill the safe zone, GDI will ellipsize on the end.
            (safe_left, safe_right)
        } else {
            // Centre on the bar; clamp the rect so it doesn't overlap the
            // pills or the clock area when the title sits off-centre.
            let bar_centre = (rc.left + rc.right) / 2;
            let ideal_left = bar_centre - title_w / 2;
            let ideal_right = ideal_left + title_w;
            (ideal_left.max(safe_left), ideal_right.min(safe_right))
        };
        let title_rect = RECT {
            left: tl,
            top: rc.top,
            right: tr,
            bottom: rc.bottom,
        };
        // SAFETY: hdc valid; font selected for the whole draw_bar call.
        unsafe {
            draw_centered_text(
                hdc,
                title_rect,
                &snap.focused_title,
                colors.foreground,
                DT_END_ELLIPSIS,
            )
        };
    }

    // ---- right indicator (e.g. "PAUSED") in the active-pill colour ----
    if let (Some(label), Some((left, right))) = (snap.right_label.as_ref(), label_box) {
        let rect = RECT {
            left,
            top: rc.top,
            right,
            bottom: rc.bottom,
        };
        // SAFETY: hdc valid; font selected for the whole draw_bar call.
        unsafe { draw_centered_text(hdc, rect, label, colors.active_bg, DRAW_TEXT_FORMAT(0)) };
    }

    // ---- live clock (right edge, always shown) ----
    let clock_rect = RECT {
        left: clock_left,
        top: rc.top,
        right: clock_right,
        bottom: rc.bottom,
    };
    // SAFETY: hdc valid; font selected for the whole draw_bar call.
    unsafe {
        draw_centered_text(
            hdc,
            clock_rect,
            &clock_str,
            colors.foreground,
            DRAW_TEXT_FORMAT(0),
        )
    };

    // ---- battery (left of clock, only when device has a battery) ----
    if let (Some(s), Some(left), Some(right)) =
        (battery_str.as_ref(), battery_zone_left, battery_zone_right)
    {
        let rect = RECT {
            left,
            top: rc.top,
            right,
            bottom: rc.bottom,
        };
        // SAFETY: hdc valid; font selected for the whole draw_bar call.
        unsafe { draw_centered_text(hdc, rect, s, colors.foreground, DRAW_TEXT_FORMAT(0)) };
    }

    // ---- network indicator (left of battery, only when an adapter is up) ----
    if let (Some(s), Some(left), Some(right)) =
        (network_str.as_ref(), network_zone_left, network_zone_right)
    {
        let rect = RECT {
            left,
            top: rc.top,
            right,
            bottom: rc.bottom,
        };
        // SAFETY: hdc valid; font selected for the whole draw_bar call.
        unsafe { draw_centered_text(hdc, rect, s, colors.foreground, DRAW_TEXT_FORMAT(0)) };
    }

    // Restore the DC's original font selection. The cache owns the font
    // handle we selected in; do NOT DeleteObject it.
    unsafe { SelectObject(hdc, old_font) };
}

/// Snapshot of the device's battery state from `GetSystemPowerStatus`.
#[derive(Debug, Clone, Copy)]
struct BatteryStatus {
    /// 0..=100, or 255 if unknown.
    percent: u8,
    /// True if `ACLineStatus == 1` (charger plugged in).
    on_ac: bool,
}

/// Read the current battery state, or `None` for devices without a battery
/// (BatteryFlag bit 7 — e.g. desktop / docked workstation) or if the call
/// itself fails (which is exceedingly rare on Windows 10+).
fn current_battery() -> Option<BatteryStatus> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: status is a valid out-param; the call is safe from any context.
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return None;
    }
    // BatteryFlag bit 7 (0x80) => "no system battery".
    if status.BatteryFlag & 0x80 != 0 {
        return None;
    }
    Some(BatteryStatus {
        percent: status.BatteryLifePercent,
        on_ac: status.ACLineStatus == 1,
    })
}

/// Format a `BatteryStatus` as `"<glyph> <percent>%"`. Uses a lightning
/// bolt when on AC, otherwise a generic battery glyph; both are BMP code
/// points present in the default Segoe UI Symbol fallback chain on every
/// Windows 10 and 11 build.
fn format_battery(b: BatteryStatus) -> String {
    let glyph = if b.on_ac { '\u{26A1}' } else { '\u{1F50B}' };
    if b.percent == 255 {
        format!("{glyph} \u{2014}")
    } else {
        format!("{glyph} {}%", b.percent)
    }
}

/// Active network interface kind. `Ethernet` wins over `WiFi` when both are
/// up because Windows itself prefers Ethernet's lower metric route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkKind {
    Ethernet,
    WiFi,
}

/// TTL for the `current_network` cache. `GetAdaptersAddresses` walks every
/// virtual + physical NIC on the box and allocates a multi-KB buffer; on a
/// Docker / Hyper-V / WSL2 host that's a non-trivial call. We were running
/// it on every `WM_PAINT` (~1 Hz from the clock alone). 5 s of staleness
/// is invisible for an indicator that only distinguishes ethernet vs wifi
/// vs offline.
const NETWORK_CACHE_TTL: Duration = Duration::from_secs(5);

/// Last network lookup result + its timestamp. `None` outer = not yet
/// queried; `None` inner = queried and found no active adapter.
static NETWORK_CACHE: Mutex<Option<(Instant, Option<NetworkKind>)>> = Mutex::new(None);

/// `current_network` wrapped with a 5-second TTL cache. Called from every
/// `draw_bar`; the cached value is returned for the vast majority of paints.
fn current_network_cached() -> Option<NetworkKind> {
    let now = Instant::now();
    {
        let guard = NETWORK_CACHE.lock();
        if let Some((at, val)) = guard.as_ref()
            && now.duration_since(*at) < NETWORK_CACHE_TTL
        {
            return *val;
        }
    }
    // Cache miss / stale: drop the lock before the slow call so a concurrent
    // paint on another thread can still serve a stale value. The bar thread
    // is currently single-threaded, but releasing the lock keeps this
    // robust if that ever changes.
    let fresh = current_network();
    *NETWORK_CACHE.lock() = Some((now, fresh));
    fresh
}

/// Walk `GetAdaptersAddresses` and return whichever active adapter type the
/// user is most likely "using". Ignores adapters that aren't `IfOperStatusUp`,
/// and any IfType other than Ethernet (6) or 802.11 WiFi (71) so that
/// loopback, tunnels, etc. don't trigger a misleading icon.
///
/// **Caveat:** virtual Ethernet adapters from Docker / Hyper-V / WSL2 / VPNs
/// also report `IF_TYPE_ETHERNET_CSMACD`, so a WiFi-only laptop running
/// Docker will show the Ethernet icon. Filtering those out reliably would
/// require `GetBestInterfaceEx`; deferred for v1.
///
/// **Why no WiFi SSID label?** Reading the SSID via `WlanQueryInterface`
/// triggers the OS-level "App is using your location" prompt because
/// Windows treats raw SSID access as a location signal (WiFi databases
/// can be reverse-geocoded). The non-location-gated alternative,
/// `INetworkListManager`, is not exposed by `windows-rs 0.58` (the version
/// pinned in this workspace), and bumping to 0.62+ requires a wider API
/// migration we deferred for v1. So the bar shows a glyph only and the
/// network name lives in the system tray's network flyout where the user
/// already expects it.
fn current_network() -> Option<NetworkKind> {
    const AF_UNSPEC: u32 = 0;
    const NO_ERROR: u32 = 0;
    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GET_ADAPTERS_ADDRESSES_FLAGS(
        GAA_FLAG_SKIP_ANYCAST.0
            | GAA_FLAG_SKIP_MULTICAST.0
            | GAA_FLAG_SKIP_DNS_SERVER.0
            | GAA_FLAG_SKIP_FRIENDLY_NAME.0,
    );

    // Pass 1: ask for the required buffer size.
    let mut size: u32 = 0;
    // SAFETY: passing None for both reserved and adapteraddresses is the
    // documented \"size query\" pattern; size is a valid out-param.
    let rc1 = unsafe { GetAdaptersAddresses(AF_UNSPEC, flags, None, None, &mut size) };
    if rc1 != ERROR_BUFFER_OVERFLOW || size == 0 {
        return None;
    }

    // Pass 2: fill the buffer.
    let mut buf = vec![0u8; size as usize];
    let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: buffer is sized per pass 1; head is a valid out-pointer.
    let rc2 = unsafe { GetAdaptersAddresses(AF_UNSPEC, flags, None, Some(head), &mut size) };
    if rc2 != NO_ERROR {
        return None;
    }

    let mut found_ethernet = false;
    let mut found_wifi = false;
    let mut cursor: *const IP_ADAPTER_ADDRESSES_LH = head;
    while !cursor.is_null() {
        // SAFETY: the OS allocates a linked list inside our buffer; each
        // node is valid until we drop `buf`, which outlives this loop.
        let adapter = unsafe { &*cursor };
        if adapter.OperStatus == IfOperStatusUp {
            match adapter.IfType {
                IF_TYPE_ETHERNET_CSMACD => found_ethernet = true,
                IF_TYPE_IEEE80211 => found_wifi = true,
                _ => {}
            }
        }
        cursor = adapter.Next;
    }

    if found_ethernet {
        Some(NetworkKind::Ethernet)
    } else if found_wifi {
        Some(NetworkKind::WiFi)
    } else {
        None
    }
}

/// Glyph-only string for the network indicator. Uses BMP-friendly emoji
/// that render in the default Segoe UI Symbol fallback chain. See
/// [`current_network`] for why we deliberately don't include the SSID /
/// adapter name (location-permission prompt avoidance).
fn format_network(kind: NetworkKind) -> String {
    match kind {
        NetworkKind::Ethernet => "\u{1F310}".to_string(), // globe with meridians
        NetworkKind::WiFi => "\u{1F4F6}".to_string(),     // antenna bars
    }
}

/// Render `s` centred within `rect` on a single vertically-centred line,
/// using the currently-selected font. `color` is a Win32 COLORREF
/// (`0x00BBGGRR`). `extra_flags` is OR-ed onto the baseline
/// `DT_CENTER | DT_VCENTER | DT_SINGLELINE` \u2014 typically `0`, or
/// `DT_END_ELLIPSIS` for text that may need to be truncated.
///
/// Centralises three things every bar element used to do by hand:
///   1. Convert the `&str` to a NUL-less `Vec<u16>` (the trailing NUL
///      otherwise becomes an invisible glyph that throws off `DT_CENTER`).
///   2. Pick a text colour.
///   3. Hand a fresh mutable `RECT` to `DrawTextW` (the call mutates it).
unsafe fn draw_centered_text(
    hdc: HDC,
    rect: RECT,
    s: &str,
    color: u32,
    extra_flags: DRAW_TEXT_FORMAT,
) {
    // SAFETY: caller has selected a valid font into `hdc`.
    unsafe { SetTextColor(hdc, COLORREF(color)) };
    let mut wide = utf16_owned(s);
    let mut text_rc = rect;
    // SAFETY: hdc valid; wide and text_rc are valid out-params.
    unsafe {
        DrawTextW(
            hdc,
            &mut wide,
            &mut text_rc,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | extra_flags,
        );
    }
}

fn measure_text(hdc: HDC, s: &str) -> i32 {
    let mut wide = utf16_owned(s);
    let mut rc = RECT::default();
    // DT_CALCRECT mutates rc to the bounding box but does not draw.
    // SAFETY: hdc valid; rc and wide are valid.
    unsafe {
        DrawTextW(
            hdc,
            &mut wide,
            &mut rc,
            windows::Win32::Graphics::Gdi::DT_CALCRECT
                | windows::Win32::Graphics::Gdi::DT_SINGLELINE,
        );
    }
    rc.right - rc.left
}

// ---- helpers ---------------------------------------------------------------

#[inline]
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}

/// Convert a `&str` into a `Vec<u16>` for `DrawTextW`. **No trailing NUL**:
/// `DrawTextW`'s third argument is the buffer length, so a NUL would be
/// counted as an extra (invisible) character and `DT_CENTER` would shift
/// the visible glyphs left to make room for it \u2014 producing the classic
/// "highlight not centred on the number" look.
fn utf16_owned(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
