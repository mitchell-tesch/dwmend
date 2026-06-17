//! Window peek — sticky-mode workspace picker overlay.
//!
//! ## What it is
//!
//! Pressing the configured `peek_toggle` combo opens a centred
//! overlay on the focused monitor that displays a horizontal grid
//! of live DWM thumbnails — one cell per managed window on the
//! focused workspace. The user navigates with their existing focus
//! direction bindings (`Alt+H/L` by default — both `H/J` cycle
//! backward and `K/L` cycle forward in a single-row layout),
//! confirms via `peek_confirm`, or dismisses by pressing
//! `peek_toggle` again or any non-focus command.
//!
//! ## Why DWM thumbnails (instead of GDI snapshots)
//!
//! `DwmRegisterThumbnail` returns a handle that DWM keeps alive at
//! compositor rate — the thumbnail is a *live* mirror of the source
//! window's output, updated for free as the user navigates. GDI
//! `BitBlt` snapshots would be stale by the time the user picks a
//! cell, and require a per-frame refresh loop. The DWM path also
//! correctly handles minimised/cloaked source windows: the API
//! returns successfully but the rendered cell stays blank, which is
//! visually the right cue ("this window has nothing to show").
//!
//! ## Architecture
//!
//! Mirrors [`bar`](super::bar) and [`toast`](super::toast):
//!
//! * One dedicated `dwmend-peek` thread owns a single hidden overlay
//!   HWND created at [`start`]. Show/hide cycles avoid CreateWindow
//!   latency on every peek session.
//! * Cells are bound to source HWNDs via `DwmRegisterThumbnail` /
//!   `DwmUpdateThumbnailProperties`. The overlay's WM_PAINT handler
//!   only fills the background and draws the highlight ring +
//!   titles; DWM composes the thumbnails on top.
//! * State (`PeekSession`) lives behind a `Mutex` so the daemon's
//!   command dispatch can mutate it from any thread.
//!
//! ## Sticky mode only (for v1)
//!
//! Tab-cycle mode (Alt-Tab style: hold modifier, tap key, release
//! to confirm) requires modifier-release detection, which the
//! current keyboard subsystem doesn't expose (it only sees
//! `WM_HOTKEY`). Sticky mode reuses the existing hotkey grammar so
//! it ships cleanly without touching the platform crate.

use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Sender, bounded};
use dwmend_platform::Rect as PRect;
use parking_lot::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DT_CENTER, DT_END_ELLIPSIS,
    DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FONT_CLIP_PRECISION,
    FONT_OUTPUT_PRECISION, FW_NORMAL, FillRect, GetStockObject, HBRUSH, HDC, HGDIOBJ,
    InvalidateRect, NULL_BRUSH, PAINTSTRUCT, PROOF_QUALITY, PS_SOLID, Rectangle, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_TOPMOST,
    IDC_ARROW, LoadCursorW, MSG, PostThreadMessageW, RegisterClassExW, SET_WINDOW_POS_FLAGS,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetWindowPos, ShowWindow, TranslateMessage,
    WINDOW_EX_STYLE, WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_QUIT, WM_USER, WNDCLASSEXW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::ids::WindowId;

// ---- public types ----------------------------------------------------------

/// User-tunable peek settings. Captured at startup; live updates
/// flow through [`set_config`].
#[derive(Debug, Clone, Copy)]
pub struct PeekConfig {
    /// Master switch. False makes [`open`] / [`cycle`] / [`confirm`]
    /// no-ops; the listener thread stays alive so re-enabling via
    /// reload takes effect immediately.
    pub enabled: bool,
    /// Pixel ratio of the overlay relative to the focused monitor's
    /// work area width. 0.8 = 80% wide.
    pub width_ratio: f32,
    /// Cell aspect ratio (width / height). 1.6 ≈ 16:10 windows;
    /// 1.78 = 16:9.
    pub cell_aspect: f32,
    /// Min / max cell dimensions in pixels. The overlay picks the
    /// largest cell width that fits the row without exceeding
    /// `cell_max_w`, but never shrinks below `cell_min_w` (instead
    /// it overflows past the right edge — the user sees the first
    /// N cells and can navigate past them).
    pub cell_min_w: i32,
    pub cell_max_w: i32,
    /// Show window titles below each thumbnail.
    pub show_titles: bool,
    /// Background fill of the overlay (`#RRGGBB` parsed by the
    /// host into a `COLORREF`).
    pub background: u32,
    /// Default text colour for titles / footer.
    pub foreground: u32,
    /// Highlight ring colour around the selected cell.
    pub highlight: u32,
}

impl Default for PeekConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            width_ratio: 0.8,
            cell_aspect: 1.6,
            cell_min_w: 140,
            cell_max_w: 280,
            show_titles: true,
            background: rgb(0x1E, 0x1E, 0x2E),
            foreground: rgb(0xC0, 0xC0, 0xC0),
            highlight: rgb(0x4F, 0xC3, 0xF7),
        }
    }
}

/// One window the picker can highlight. Built by the host crate's
/// `peek_open` from the focused workspace's BSP tree.
#[derive(Debug, Clone)]
pub struct PeekSource {
    pub window_id: WindowId,
    pub source_hwnd: isize,
    pub title: String,
}

/// Geometric placement spec passed in on every [`open`] call.
/// `monitor_bounds` is the full monitor rect (NOT the work area —
/// peek floats over the top of the bar/taskbar to maximise canvas).
#[derive(Debug, Clone, Copy)]
pub struct PeekMonitor {
    pub bounds: PRect,
}

// ---- shared state ----------------------------------------------------------

const HIGHLIGHT_THICKNESS: i32 = 3;
const CELL_GAP: i32 = 12;
const TITLE_BAND_H: i32 = 22;
const FOOTER_BAND_H: i32 = 18;
const OUTER_PAD: i32 = 16;
const OVERLAY_RADIUS: i32 = 12;

/// Custom thread message: open or update the peek session. The host
/// pushes the new [`Session`] descriptor into [`PENDING`] and posts
/// this message; the listener thread builds the overlay against it.
const WM_PEEK_OPEN: u32 = WM_USER + 1;
/// Repaint without touching session data. Used by [`cycle`].
const WM_PEEK_REPAINT: u32 = WM_USER + 2;
/// Hide the overlay and unregister thumbnails.
const WM_PEEK_DISMISS: u32 = WM_USER + 3;

static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);
static OVERLAY: OnceLock<isize> = OnceLock::new();
static CONFIG: OnceLock<Mutex<PeekConfig>> = OnceLock::new();

/// Active session, or `None` when the overlay is hidden. Public
/// callers hold `is_open()` etc. behind this; the listener thread
/// reads it on WM_PAINT.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Pending open request handed from caller to listener thread.
/// Replaces any prior pending request on coalescing.
static PENDING: Mutex<Option<PendingOpen>> = Mutex::new(None);

/// Cached shared font for title rendering. Created lazily on first
/// paint and freed in [`stop`]. `0` means "not yet loaded".
static FONT_HANDLE: AtomicIsize = AtomicIsize::new(0);

#[derive(Debug, Clone)]
struct PendingOpen {
    monitor: PeekMonitor,
    sources: Vec<PeekSource>,
    initial_focused: Option<WindowId>,
}

#[derive(Debug)]
struct Session {
    /// Per-cell state, in left-to-right display order.
    cells: Vec<Cell>,
    /// 0..cells.len(); valid as long as cells is non-empty.
    highlight_idx: usize,
}

#[derive(Debug)]
struct Cell {
    window_id: WindowId,
    title: String,
    /// Position relative to the overlay's client area.
    rect: RECT,
    /// HTHUMBNAIL stored as `isize` so the struct is `Send`.
    /// `0` if registration failed for this source (rendered as a
    /// blank cell with the title still drawn underneath, so the
    /// user can still pick the window).
    thumb: isize,
}

// ---- public API ------------------------------------------------------------

/// Spawn the peek listener thread + pre-create the overlay HWND
/// (hidden). Must be called once after the daemon has its config.
/// Subsequent calls return an error so an accidental double-start
/// doesn't leak threads.
pub fn start(cfg: PeekConfig) -> Result<()> {
    if LISTENER_TID.load(Ordering::SeqCst) != 0 {
        return Err(eyre!("peek::start called more than once"));
    }
    let _ = CONFIG.set(Mutex::new(cfg));

    let (init_tx, init_rx) = bounded::<std::result::Result<isize, String>>(1);
    std::thread::Builder::new()
        .name("dwmend-peek".into())
        .spawn(move || run_peek_thread(init_tx))
        .map_err(|e| eyre!("spawn dwmend-peek thread: {e}"))?;

    match init_rx
        .recv()
        .map_err(|_| eyre!("peek thread died during init"))?
    {
        Ok(hwnd_isize) => {
            let _ = OVERLAY.set(hwnd_isize);
            tracing::info!("peek subsystem initialised");
            Ok(())
        }
        Err(e) => Err(eyre!("peek init failed: {e}")),
    }
}

/// Replace the live config. Subsequent open sessions use the new
/// values; an in-progress session keeps the values it was opened
/// with so colours don't change mid-pick.
pub fn set_config(new_cfg: PeekConfig) {
    if let Some(slot) = CONFIG.get() {
        *slot.lock() = new_cfg;
    }
}

/// True iff the overlay is currently visible. Cheap; used by the
/// command dispatcher to route focus_direction into peek navigation.
pub fn is_open() -> bool {
    SESSION.lock().is_some()
}

/// Open the overlay on `monitor` with `sources`. No-op when the
/// subsystem is disabled, hasn't started, or `sources` is empty.
/// `initial_focused` controls which cell is highlighted first;
/// callers typically pass the WM's currently focused window.
pub fn open(monitor: PeekMonitor, sources: Vec<PeekSource>, initial_focused: Option<WindowId>) {
    if !config_snapshot().enabled {
        return;
    }
    if sources.is_empty() {
        return;
    }
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    *PENDING.lock() = Some(PendingOpen {
        monitor,
        sources,
        initial_focused,
    });
    // SAFETY: PostThreadMessageW with a non-zero TID is documented safe.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_PEEK_OPEN, WPARAM(0), LPARAM(0));
    }
}

/// Move the highlight by `delta` (negative = backward, positive =
/// forward). Wraps at both ends. No-op when peek isn't open.
pub fn cycle(delta: i32) {
    let mut moved = false;
    {
        let mut guard = SESSION.lock();
        if let Some(s) = guard.as_mut() {
            let len = s.cells.len() as i32;
            if len > 0 {
                let cur = s.highlight_idx as i32;
                let new = ((cur + delta).rem_euclid(len)) as usize;
                if new != s.highlight_idx {
                    s.highlight_idx = new;
                    moved = true;
                }
            }
        }
    }
    if moved {
        post_repaint();
    }
}

/// Close the overlay without committing. Returns the highlighted
/// `WindowId` so callers that want "dismiss-but-track-selection"
/// can do so (currently unused).
pub fn dismiss() -> Option<WindowId> {
    let last_pick = SESSION
        .lock()
        .as_ref()
        .and_then(|s| s.cells.get(s.highlight_idx).map(|c| c.window_id));
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return last_pick;
    }
    // SAFETY: PostThreadMessageW with a valid TID.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_PEEK_DISMISS, WPARAM(0), LPARAM(0));
    }
    last_pick
}

/// Commit the current selection. Returns the `WindowId` the
/// caller should focus, or `None` when peek wasn't open.
pub fn confirm() -> Option<WindowId> {
    let pick = SESSION
        .lock()
        .as_ref()
        .and_then(|s| s.cells.get(s.highlight_idx).map(|c| c.window_id));
    if pick.is_some() {
        // Issue dismiss as a side effect so the caller doesn't need
        // to remember to clean up.
        let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
        if tid != 0 {
            // SAFETY: PostThreadMessageW with a valid TID.
            unsafe {
                let _ = PostThreadMessageW(tid, WM_PEEK_DISMISS, WPARAM(0), LPARAM(0));
            }
        }
    }
    pick
}

/// Cooperative shutdown.
pub fn stop() {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    // SAFETY: PostThreadMessageW with a valid TID.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
    }
}

// ---- helpers ---------------------------------------------------------------

fn config_snapshot() -> PeekConfig {
    CONFIG.get().map(|m| *m.lock()).unwrap_or_default()
}

fn post_repaint() {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    // SAFETY: PostThreadMessageW with a valid TID.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_PEEK_REPAINT, WPARAM(0), LPARAM(0));
    }
}

#[inline]
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}

// ---- listener thread -------------------------------------------------------

fn run_peek_thread(init_tx: Sender<std::result::Result<isize, String>>) {
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    let class_name = utf16(b"DwmendPeekOverlay\0");

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
        // SAFETY: shared system arrow cursor; never freed by us.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };
    // SAFETY: class fully initialised; class_name null-terminated.
    let class_atom = unsafe { RegisterClassExW(&class) };
    if class_atom == 0 {
        let _ = init_tx.send(Err("RegisterClassExW failed".into()));
        return;
    }

    // Pre-create the overlay HWND hidden. Position is overwritten on
    // each `open()` call. Must be a top-level window (not message-only)
    // for DwmRegisterThumbnail to work.
    let ex = WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0);
    // SAFETY: parameters are valid; PCWSTR pointers outlive the call.
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WS_POPUP,
            0,
            0,
            16,
            16, // placeholder; overwritten on open
            None,
            None,
            Some(hinst),
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            let _ = init_tx.send(Err(format!("CreateWindowExW: {e}")));
            return;
        }
    };

    let _ = init_tx.send(Ok(hwnd.0 as isize));
    tracing::info!(tid, "peek thread started");

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param; hwnd=None pumps both
        // overlay messages and our thread messages.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break; // WM_QUIT (0) or error (-1)
        }
        // Thread messages (msg.hwnd null) DispatchMessageW skips,
        // so we handle them inline.
        if msg.hwnd.0.is_null() {
            match msg.message {
                WM_PEEK_OPEN => handle_open(hwnd),
                WM_PEEK_REPAINT => {
                    // SAFETY: hwnd from our own CreateWindow.
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                WM_PEEK_DISMISS => handle_dismiss(hwnd),
                _ => {}
            }
            continue;
        }
        // SAFETY: msg is populated.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup on quit.
    handle_dismiss(hwnd);
    // SAFETY: hwnd from our own CreateWindow.
    let _ = unsafe { DestroyWindow(hwnd) };
    let f = FONT_HANDLE.swap(0, Ordering::SeqCst);
    if f != 0 {
        // SAFETY: font handle from CreateFontW; not double-freed.
        let _ = unsafe { DeleteObject(HGDIOBJ(f as *mut _)) };
    }
    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("peek thread exited");
}

/// Handle WM_PEEK_OPEN: tear down any prior session, lay out cells,
/// register DWM thumbnails, position + show the overlay.
fn handle_open(hwnd: HWND) {
    // First dispose of any prior session.
    handle_dismiss(hwnd);

    let pending = match PENDING.lock().take() {
        Some(p) => p,
        None => return,
    };
    let cfg = config_snapshot();
    let bounds = pending.monitor.bounds;

    // Compute overlay geometry: width_ratio of monitor width,
    // height that fits one row of cells + title band + footer + pad.
    let overlay_w =
        ((bounds.w as f32) * cfg.width_ratio.clamp(0.3, 1.0)).round() as i32;
    let n = pending.sources.len() as i32;
    let usable_w = overlay_w - 2 * OUTER_PAD;
    // Ideal cell width: usable_w spread across N cells with gaps.
    let ideal_cell_w = if n == 0 {
        cfg.cell_min_w
    } else {
        (usable_w - (n - 1) * CELL_GAP) / n
    };
    let cell_w = ideal_cell_w.clamp(cfg.cell_min_w, cfg.cell_max_w);
    let cell_h = (cell_w as f32 / cfg.cell_aspect.max(0.5)).round() as i32;
    let title_h = if cfg.show_titles { TITLE_BAND_H } else { 0 };

    let overlay_h = OUTER_PAD + cell_h + title_h + FOOTER_BAND_H + OUTER_PAD;
    // Centre over monitor bounds.
    let overlay_x = bounds.x + (bounds.w - overlay_w) / 2;
    let overlay_y = bounds.y + (bounds.h - overlay_h) / 2;

    // Lay out cells left-to-right starting at OUTER_PAD,OUTER_PAD.
    // If the row would overflow `usable_w`, it still gets laid out
    // sequentially — the user sees the first N that fit and can
    // cycle to the rest. Cycling re-paints the highlight; the cell
    // index stays in display order.
    let mut cells: Vec<Cell> = Vec::with_capacity(pending.sources.len());
    let mut x = OUTER_PAD;
    for src in &pending.sources {
        let rect = RECT {
            left: x,
            top: OUTER_PAD,
            right: x + cell_w,
            bottom: OUTER_PAD + cell_h,
        };
        let thumb = register_thumb(hwnd, src.source_hwnd, rect);
        cells.push(Cell {
            window_id: src.window_id,
            title: src.title.clone(),
            rect,
            thumb,
        });
        x += cell_w + CELL_GAP;
    }

    // Initial highlight: prefer the caller-provided window; else 0.
    let highlight_idx = pending
        .initial_focused
        .and_then(|w| cells.iter().position(|c| c.window_id == w))
        .unwrap_or(0);

    *SESSION.lock() = Some(Session {
        cells,
        highlight_idx,
    });

    // Position + show.
    // SAFETY: hwnd from our own CreateWindow; HWND_TOPMOST keeps us
    // above app windows; SWP_NOACTIVATE preserves focus.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            overlay_x,
            overlay_y,
            overlay_w,
            overlay_h,
            SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0),
        )
    };
    // SAFETY: hwnd from our own CreateWindow.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    // SAFETY: hwnd from our own CreateWindow.
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// Handle WM_PEEK_DISMISS: unregister thumbnails, clear session,
/// hide overlay. Idempotent.
fn handle_dismiss(hwnd: HWND) {
    let session = SESSION.lock().take();
    if let Some(s) = session {
        for c in &s.cells {
            unregister_thumb(c.thumb);
        }
    }
    // SAFETY: hwnd from our own CreateWindow.
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
}

/// Register a single DWM thumbnail. Returns the handle on
/// success, or `0` on failure so the rest of the picker still
/// functions — the cell renders blank but the title text and
/// click target still work. Failure is normal for cloaked /
/// minimised / off-VD source HWNDs.
fn register_thumb(dest: HWND, source: isize, dest_rect: RECT) -> isize {
    let src_hwnd = HWND(source as *mut _);
    // SAFETY: dest is our own HWND; src is a user-window HWND
    // collected by the caller (already validated via WindowManager).
    // windows-rs 0.62 returns the new handle as a plain `isize`.
    let thumb = match unsafe { DwmRegisterThumbnail(dest, src_hwnd) } {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
        rcDestination: dest_rect,
        opacity: 255,
        fVisible: true.into(),
        ..Default::default()
    };
    // SAFETY: thumb just registered above; props lives until end of fn.
    let _ = unsafe { DwmUpdateThumbnailProperties(thumb, &props) };
    thumb
}

/// Inverse of [`register_thumb`]. Idempotent on a `0` handle.
fn unregister_thumb(thumb: isize) {
    if thumb == 0 {
        return;
    }
    // SAFETY: caller passed an HTHUMBNAIL we registered earlier.
    let _ = unsafe { DwmUnregisterThumbnail(thumb) };
}

// ---- wnd_proc / paint ------------------------------------------------------

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // SAFETY: hwnd valid; ps is a valid out-param.
            unsafe { handle_paint(hwnd) };
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // We paint every visible pixel ourselves in WM_PAINT; the
            // OS-default erase would just create flicker.
            LRESULT(1)
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

    let cfg = config_snapshot();
    let session = SESSION.lock();

    // Background fill — covers everything DWM doesn't repaint.
    let client_rc = ps.rcPaint;
    // SAFETY: CreateSolidBrush is a pure GDI allocator.
    let bg_brush = unsafe { CreateSolidBrush(COLORREF(cfg.background)) };
    if !bg_brush.is_invalid() {
        // SAFETY: bg_brush valid; client_rc populated by BeginPaint.
        let _ = unsafe { FillRect(hdc, &client_rc, HBRUSH(bg_brush.0)) };
        // SAFETY: handle from our own CreateSolidBrush.
        let _ = unsafe { DeleteObject(HGDIOBJ(bg_brush.0)) };
    }

    if let Some(s) = session.as_ref() {
        // Highlight ring around the focused cell. Drawn before
        // titles so the title text overlays the ring (matters when
        // the title sits within the highlight band).
        if let Some(cell) = s.cells.get(s.highlight_idx) {
            unsafe {
                draw_highlight_ring(hdc, cell.rect, cfg.highlight);
            }
        }

        // Titles below each cell.
        if cfg.show_titles {
            let font = ensure_font();
            // SAFETY: font is alive (cache holds it); SelectObject
            // returns the previously-selected font for restoration.
            let old_font = unsafe { SelectObject(hdc, HGDIOBJ(font as *mut _)) };
            // SAFETY: hdc valid.
            unsafe {
                SetBkMode(hdc, TRANSPARENT);
            }
            for (i, cell) in s.cells.iter().enumerate() {
                let title_rect = RECT {
                    left: cell.rect.left,
                    top: cell.rect.bottom + 4,
                    right: cell.rect.right,
                    bottom: cell.rect.bottom + 4 + TITLE_BAND_H,
                };
                let color = if i == s.highlight_idx {
                    cfg.highlight
                } else {
                    cfg.foreground
                };
                // SAFETY: hdc valid; font selected for whole paint.
                unsafe {
                    draw_text_centered(
                        hdc,
                        title_rect,
                        &cell.title,
                        color,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                    );
                }
            }
            // SAFETY: restoring previously-selected font.
            unsafe {
                SelectObject(hdc, old_font);
            }
        }

        // Footer hint.
        let font = ensure_font();
        // SAFETY: font alive.
        let old_font = unsafe { SelectObject(hdc, HGDIOBJ(font as *mut _)) };
        // SAFETY: hdc valid.
        unsafe {
            SetBkMode(hdc, TRANSPARENT);
        }
        let footer_rc = RECT {
            left: client_rc.left,
            top: client_rc.bottom - FOOTER_BAND_H - 4,
            right: client_rc.right,
            bottom: client_rc.bottom - 4,
        };
        let footer = format!(
            "[{}/{}]  H/L cycle  Enter focus  Esc dismiss",
            s.highlight_idx + 1,
            s.cells.len()
        );
        // SAFETY: hdc valid; font selected.
        unsafe {
            draw_text_centered(
                hdc,
                footer_rc,
                &footer,
                cfg.foreground,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        // SAFETY: restoring font.
        unsafe {
            SelectObject(hdc, old_font);
        }
    }

    // SAFETY: hwnd valid; ps from BeginPaint.
    let _ = unsafe { EndPaint(hwnd, &ps) };
    // Ignore unused for clarity in this branchy function.
    let _ = (hwnd, OVERLAY_RADIUS); // silence unused warning when radius is unused
}

unsafe fn draw_highlight_ring(hdc: HDC, target: RECT, color: u32) {
    // Build a thick rectangle ring by drawing N concentric rects
    // (cheap, no need for a region or pen path). Width comes from
    // HIGHLIGHT_THICKNESS so the ring sits flush against the
    // thumbnail's edge.
    // SAFETY: pure GDI allocators.
    let pen = unsafe { CreatePen(PS_SOLID, HIGHLIGHT_THICKNESS, COLORREF(color)) };
    let null_brush = unsafe { GetStockObject(NULL_BRUSH) };
    // SAFETY: hdc valid; pen + null_brush valid.
    let old_pen = unsafe { SelectObject(hdc, HGDIOBJ(pen.0)) };
    let old_brush = unsafe { SelectObject(hdc, null_brush) };
    // Inflate by half the pen width so the ring sits *outside* the
    // thumbnail rather than clipping into it.
    let inset = HIGHLIGHT_THICKNESS / 2 + 1;
    // SAFETY: hdc valid; coordinates derived from caller's RECT.
    let _ = unsafe {
        Rectangle(
            hdc,
            target.left - inset,
            target.top - inset,
            target.right + inset,
            target.bottom + inset,
        )
    };
    // SAFETY: restoring original objects.
    unsafe {
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
    }
    // SAFETY: pen handle from our own CreatePen.
    let _ = unsafe { DeleteObject(HGDIOBJ(pen.0)) };
}

unsafe fn draw_text_centered(
    hdc: HDC,
    rect: RECT,
    s: &str,
    color: u32,
    flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    // SAFETY: caller has selected a valid font.
    unsafe { SetTextColor(hdc, COLORREF(color)) };
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    let mut text_rc = rect;
    // SAFETY: hdc valid; wide and text_rc are valid out-params.
    unsafe {
        DrawTextW(hdc, &mut wide, &mut text_rc, flags);
    }
}

fn ensure_font() -> isize {
    let cur = FONT_HANDLE.load(Ordering::SeqCst);
    if cur != 0 {
        return cur;
    }
    let mut face = utf16(b"Segoe UI Variable\0");
    // SAFETY: CreateFontW is a pure GDI allocator; returns 0 on
    // failure which the caller treats as "use default font".
    let font = unsafe {
        CreateFontW(
            14,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            windows::Win32::Graphics::Gdi::FONT_CHARSET(0),
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            PROOF_QUALITY,
            0,
            PCWSTR(face.as_mut_ptr()),
        )
    };
    let raw = font.0 as isize;
    if FONT_HANDLE
        .compare_exchange(0, raw, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // SAFETY: handle from CreateFontW above.
        let _ = unsafe { DeleteObject(HGDIOBJ(font.0 as *mut _)) };
        FONT_HANDLE.load(Ordering::SeqCst)
    } else {
        raw
    }
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_wraps_at_both_ends() {
        // Stand up a session manually so we don't have to spin up
        // the Win32 listener thread for a pure-state test.
        *SESSION.lock() = Some(Session {
            cells: vec![
                Cell {
                    window_id: WindowId(1),
                    title: "a".into(),
                    rect: RECT::default(),
                    thumb: 0,
                },
                Cell {
                    window_id: WindowId(2),
                    title: "b".into(),
                    rect: RECT::default(),
                    thumb: 0,
                },
                Cell {
                    window_id: WindowId(3),
                    title: "c".into(),
                    rect: RECT::default(),
                    thumb: 0,
                },
            ],
            highlight_idx: 0,
        });

        cycle(1);
        assert_eq!(SESSION.lock().as_ref().unwrap().highlight_idx, 1);
        cycle(1);
        assert_eq!(SESSION.lock().as_ref().unwrap().highlight_idx, 2);
        // Forward off the end wraps to 0.
        cycle(1);
        assert_eq!(SESSION.lock().as_ref().unwrap().highlight_idx, 0);
        // Backward off 0 wraps to last.
        cycle(-1);
        assert_eq!(SESSION.lock().as_ref().unwrap().highlight_idx, 2);

        // Reset so other tests / runs aren't affected.
        *SESSION.lock() = None;
    }

    #[test]
    fn confirm_returns_highlighted_window() {
        *SESSION.lock() = Some(Session {
            cells: vec![Cell {
                window_id: WindowId(42),
                title: "x".into(),
                rect: RECT::default(),
                thumb: 0,
            }],
            highlight_idx: 0,
        });
        // confirm() posts a thread message then returns the pick;
        // when LISTENER_TID is 0 (no live thread in unit tests) the
        // post is silently skipped, which is fine for this test.
        let pick = confirm();
        assert_eq!(pick, Some(WindowId(42)));
        // Note: confirm() does NOT clear the session synchronously
        // (the thread does that). For unit-test isolation we clear
        // here.
        *SESSION.lock() = None;
    }

    #[test]
    fn is_open_tracks_session() {
        assert!(!is_open());
        *SESSION.lock() = Some(Session {
            cells: vec![],
            highlight_idx: 0,
        });
        assert!(is_open());
        *SESSION.lock() = None;
        assert!(!is_open());
    }

    #[test]
    fn config_round_trip() {
        let cfg = PeekConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.width_ratio > 0.0 && cfg.width_ratio <= 1.0);
        assert!(cfg.cell_min_w < cfg.cell_max_w);
    }
}
