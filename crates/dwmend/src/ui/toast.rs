//! Notification toasts — transient feedback overlays.
//!
//! ## Why custom (and not `Shell_NotifyIcon` toasts)
//!
//! Native Windows toasts via `IToastNotification` require a registered
//! AppUserModelID, an installed AppX/MSIX manifest, and a per-toast XML
//! payload. Overkill for a window manager that just needs to say
//! "config reloaded" or "keybinding failed". Custom layered popups
//! reusing the [`bar`](super::bar) / [`focus_border`] infrastructure
//! are small, dependency-free, and match the rest of DWMend's UI
//! aesthetic.
//!
//! [`focus_border`]: dwmend_platform::focus_border
//!
//! ## Architecture
//!
//! One dedicated thread (`dwmend-toast`) owns every toast HWND across
//! every monitor. Show requests arrive via `PostThreadMessageW(tid,
//! WM_TOAST_SHOW)` after the caller pushes a [`ShowRequest`] onto the
//! shared queue. A per-toast 30 Hz `WM_TIMER` drives fade-in / hold /
//! fade-out and triggers `DestroyWindow` when the lifetime expires.
//!
//! Each toast is a `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT |
//! WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` popup — click-through, never
//! activated, never in Alt-Tab. Window-level alpha is set via
//! `SetLayeredWindowAttributes(LWA_ALPHA)`; the regular `WM_PAINT`
//! handler renders the rounded background, severity glyph, and message
//! text using GDI.

use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Sender, bounded};
use dwmend_platform::Rect;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DT_END_ELLIPSIS, DT_LEFT,
    DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FONT_CLIP_PRECISION,
    FONT_OUTPUT_PRECISION, FW_NORMAL, FillRect, HBRUSH, HDC, HGDIOBJ, PAINTSTRUCT,
    PROOF_QUALITY, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_TOPMOST,
    IDC_ARROW, LWA_ALPHA, LoadCursorW, MSG, PostThreadMessageW, RegisterClassExW,
    SET_WINDOW_POS_FLAGS, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetLayeredWindowAttributes, SetTimer,
    SetWindowPos, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_DESTROY, WM_ERASEBKGND,
    WM_PAINT, WM_QUIT, WM_TIMER, WM_USER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

// ---- public types -----------------------------------------------------------

/// Severity of a notification, controlling the background colour and
/// leading glyph. `Info` is the bar's accent colour; `Warn` is amber;
/// `Error` is red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

/// Where the per-monitor toast stack anchors. Currently only
/// [`ToastAnchor::TopRight`] is implemented; the enum exists so other
/// corners can be added without breaking configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastAnchor {
    #[default]
    TopRight,
}

/// Background + foreground colours per severity, as Win32 `COLORREF`
/// triples (`0x00BBGGRR`). The host parses `"#RRGGBB"` strings via
/// `config::parse_border_color` and packs them in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToastColors {
    pub info_bg: u32,
    pub info_fg: u32,
    pub warn_bg: u32,
    pub warn_fg: u32,
    pub error_bg: u32,
    pub error_fg: u32,
}

impl Default for ToastColors {
    fn default() -> Self {
        Self {
            info_bg: rgb(0x4F, 0xC3, 0xF7),  // sky blue
            info_fg: rgb(0x10, 0x10, 0x18),  // near-black
            warn_bg: rgb(0xF9, 0xA8, 0x25),  // amber
            warn_fg: rgb(0x10, 0x10, 0x18),  // near-black
            error_bg: rgb(0xE5, 0x39, 0x35), // red
            error_fg: rgb(0xFF, 0xFF, 0xFF), // white
        }
    }
}

/// User-tunable toast subsystem settings. Captured at startup; live
/// updates flow through [`set_config`].
#[derive(Debug, Clone, Copy)]
pub struct ToastConfig {
    /// Master switch. False suppresses every [`show`] / [`show_on`]
    /// call but keeps the listener thread alive so a config-reload can
    /// flip it back on.
    pub enabled: bool,
    /// How long a toast remains at full opacity, in milliseconds.
    /// The fade-in (~150 ms) and fade-out (~200 ms) durations are
    /// fixed and added on top, so a `ttl_ms` of 2000 produces a total
    /// visible lifetime of ~2.35 s.
    pub ttl_ms: u32,
    /// Maximum number of concurrent toasts per monitor. Beyond this,
    /// the oldest still-fading-in / holding toast is forced into
    /// fade-out so the new toast has room.
    pub max_concurrent: u32,
    /// Stack anchor corner.
    pub anchor: ToastAnchor,
    /// Severity colour palette.
    pub colors: ToastColors,
}

impl Default for ToastConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_ms: 2200,
            max_concurrent: 3,
            anchor: ToastAnchor::TopRight,
            colors: ToastColors::default(),
        }
    }
}

/// Per-monitor placement spec. The host passes one of these per
/// connected monitor at start-up and on every monitor topology
/// change, mirroring `bar::BarSpec`.
///
/// `work_area` is the rect the toast stack anchors against. The host
/// is responsible for subtracting any reserved bar height before
/// passing it in (toast `top` should land below the bar).
#[derive(Debug, Clone)]
pub struct ToastSpec {
    pub monitor_id: String,
    pub work_area: Rect,
}

// ---- shared state ----------------------------------------------------------

const TOAST_W: i32 = 320;
const TOAST_H: i32 = 40;
const TOAST_MARGIN: i32 = 12;
const TOAST_GAP: i32 = 6;
const TOAST_RADIUS: i32 = 8;

const FADE_IN_MS: u32 = 150;
const FADE_OUT_MS: u32 = 200;
const ANIM_TIMER_ID: usize = 1;
const ANIM_INTERVAL_MS: u32 = 33; // ~30 Hz

/// Custom thread messages for the toast listener thread.
const WM_TOAST_SHOW: u32 = WM_USER + 1;
const WM_TOAST_SYNC: u32 = WM_USER + 2;

/// Listener thread id. `0` until [`start`] returns.
static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);

/// Module-wide enabled flag, hot-flippable via [`set_config`]. Cheap
/// to read on every `show` call so we don't even bother locking the
/// config when toasts are off.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Most recent config, applied to subsequent toasts. Older toasts in
/// flight finish their lifetime under the config they were created
/// with (snapshotted into [`ToastWindow`] at spawn time).
static CONFIG: OnceLock<Mutex<ToastConfig>> = OnceLock::new();

/// Caller-side queue. The toast thread drains it in response to
/// `WM_TOAST_SHOW`. Bounded only by the `Vec` capacity it grows to;
/// in normal use 0..=3 entries pile up between a `show()` call and
/// the thread waking from `GetMessageW`.
static QUEUE: Mutex<VecDeque<ShowRequest>> = Mutex::new(VecDeque::new());

/// Pending spec list for the next `WM_TOAST_SYNC`. Mirrors `bar`'s
/// `PENDING_SYNC`.
static PENDING_SYNC: Mutex<Option<Vec<ToastSpec>>> = Mutex::new(None);

/// Live HWND set keyed by HWND-as-isize. The wnd_proc looks itself up
/// by HWND on every paint / timer / destroy. Vec rather than HashMap
/// because lookups are bounded by `max_concurrent * num_monitors`,
/// usually ≤ 12.
static TOASTS: Mutex<Vec<ToastWindow>> = Mutex::new(Vec::new());

/// The current set of monitor specs. The toast thread updates this on
/// `WM_TOAST_SYNC`; the spawn path reads it to position fresh toasts.
static MONITORS: Mutex<Vec<ToastSpec>> = Mutex::new(Vec::new());

/// "Default" monitor used when callers invoke [`show`] without an
/// explicit monitor id. Updated by the daemon whenever the focused
/// monitor changes (typically right before `publish_bar_state`).
static DEFAULT_MONITOR: Mutex<Option<String>> = Mutex::new(None);

/// Cached GDI font shared across every toast paint. The handle is
/// allocated lazily on the first paint and freed in [`stop`]. Stored
/// as `isize` so the static is `Send + Sync`; the cast back to
/// `HFONT` happens at the use site.
static FONT_HANDLE: AtomicIsize = AtomicIsize::new(0);

/// Per-toast state held under the global [`TOASTS`] lock.
struct ToastWindow {
    hwnd: isize,
    monitor_id: String,
    level: ToastLevel,
    text: String,
    /// Time of the `CreateWindowExW` call. Drives alpha computation.
    born: Instant,
    /// Hold-phase duration, snapshotted from the config at spawn time.
    ttl: Duration,
    /// Set once the toast has been pushed into early fade-out (either
    /// by reaching the natural end of `ttl` or by being evicted to
    /// make room for a newer toast). Used so the timer doesn't try to
    /// destroy the same window twice.
    forced_dismiss: bool,
    /// Override-`born` for fade-out start when an eviction collapses
    /// the hold phase. `None` means natural end (born + fade_in + ttl).
    fade_out_start: Option<Instant>,
    /// Snapshotted colours so a mid-flight `set_config` doesn't
    /// suddenly recolour an in-progress fade.
    colors: ToastColors,
}

struct ShowRequest {
    level: ToastLevel,
    text: String,
    monitor_id: String,
}

// ---- public API -------------------------------------------------------------

/// Spawn the toast listener thread. Safe to call once per process; a
/// second call returns an error. Failure is recoverable from the
/// host's perspective — every subsequent `show` becomes a no-op.
pub fn start(specs: Vec<ToastSpec>, cfg: ToastConfig) -> Result<()> {
    if LISTENER_TID.load(Ordering::SeqCst) != 0 {
        return Err(eyre!("toast::start called more than once"));
    }
    *MONITORS.lock() = specs;
    let _ = CONFIG.set(Mutex::new(cfg));
    ENABLED.store(cfg.enabled, Ordering::Relaxed);

    let (init_tx, init_rx) = bounded::<std::result::Result<(), String>>(1);

    std::thread::Builder::new()
        .name("dwmend-toast".into())
        .spawn(move || run_toast_thread(init_tx))
        .map_err(|e| eyre!("spawn dwmend-toast thread: {e}"))?;

    match init_rx
        .recv()
        .map_err(|_| eyre!("toast thread died during init"))?
    {
        Ok(()) => {
            tracing::info!(
                ttl_ms = cfg.ttl_ms,
                max_concurrent = cfg.max_concurrent,
                "toast subsystem initialised"
            );
            Ok(())
        }
        Err(e) => Err(eyre!("toast init failed: {e}")),
    }
}

/// Display a toast on the current default monitor (the most recent
/// argument to [`set_default_monitor`], or the first connected monitor
/// if the host hasn't set a default yet). No-op when the subsystem is
/// disabled, hasn't started, or has no known monitors.
pub fn show(level: ToastLevel, text: String) {
    let mid = {
        let g = DEFAULT_MONITOR.lock();
        g.clone().or_else(|| MONITORS.lock().first().map(|s| s.monitor_id.clone()))
    };
    let Some(mid) = mid else {
        return;
    };
    show_on(level, text, &mid);
}

/// Display a toast on a specific monitor. Use this when the caller
/// already knows which monitor should receive the message (e.g. a
/// per-monitor diagnostic).
pub fn show_on(level: ToastLevel, text: String, monitor_id: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    {
        let mut q = QUEUE.lock();
        q.push_back(ShowRequest {
            level,
            text,
            monitor_id: monitor_id.to_string(),
        });
    }
    // Wake the toast thread so it drains the queue. Failure means the
    // thread already exited — show() then becomes a silent no-op,
    // matching the docstring.
    // SAFETY: PostThreadMessageW with a still-valid TID is documented
    // as safe; if the thread already exited we get an error which we
    // intentionally ignore.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_TOAST_SHOW, WPARAM(0), LPARAM(0));
    }
}

/// Replace the live config. Subsequent toasts use the new colours,
/// TTL, and concurrency cap; in-flight toasts complete with the
/// values they were spawned under so a mid-flight reload doesn't
/// retroactively recolour a fade.
pub fn set_config(new_cfg: ToastConfig) {
    ENABLED.store(new_cfg.enabled, Ordering::Relaxed);
    if let Some(slot) = CONFIG.get() {
        *slot.lock() = new_cfg;
    }
}

/// Update the "default" monitor used by [`show`] when no explicit id
/// is passed. The daemon calls this whenever the focused monitor
/// changes — typically right before `publish_bar_state`.
pub fn set_default_monitor(monitor_id: String) {
    *DEFAULT_MONITOR.lock() = Some(monitor_id);
}

/// Reconcile the live monitor set with `specs`. Toasts whose
/// `monitor_id` is no longer in `specs` are forced into fade-out so
/// they don't linger on a now-disconnected display. New monitors
/// simply become eligible targets for future toasts.
///
/// Posts a thread message; safe to call from any thread. No-op if
/// the subsystem hasn't started.
pub fn sync_monitors(specs: Vec<ToastSpec>) {
    let tid = LISTENER_TID.load(Ordering::SeqCst) as u32;
    if tid == 0 {
        return;
    }
    *PENDING_SYNC.lock() = Some(specs);
    // SAFETY: PostThreadMessageW with a valid TID.
    unsafe {
        let _ = PostThreadMessageW(tid, WM_TOAST_SYNC, WPARAM(0), LPARAM(0));
    }
}

/// Cooperative shutdown. Posts WM_QUIT to the listener thread, which
/// destroys every active toast HWND on its way out.
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

// ---- listener thread -------------------------------------------------------

fn run_toast_thread(init_tx: Sender<std::result::Result<(), String>>) {
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    let class_name = utf16(b"DwmendToast\0");

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
        // SAFETY: LoadCursorW with HINSTANCE=NULL + IDC_ARROW returns a
        // shared system cursor we never free.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };
    // SAFETY: class is fully initialised; class_name is null-terminated.
    let class_atom = unsafe { RegisterClassExW(&class) };
    if class_atom == 0 {
        let _ = init_tx.send(Err("RegisterClassExW failed".into()));
        return;
    }

    let _ = init_tx.send(Ok(()));
    tracing::info!(tid, "toast thread started");

    let class_name_ptr = class_name.as_ptr();
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param; hwnd=None pumps every
        // toast HWND and thread message.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break; // WM_QUIT (0) or error (-1).
        }
        // Thread messages (msg.hwnd is null) DispatchMessageW skips,
        // so we handle them inline.
        if msg.hwnd.0.is_null() {
            match msg.message {
                WM_TOAST_SHOW => drain_queue(hinst, class_name_ptr),
                WM_TOAST_SYNC => {
                    if let Some(specs) = PENDING_SYNC.lock().take() {
                        apply_sync(specs);
                    }
                }
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

    // Cleanup: destroy every live toast.
    let hwnds: Vec<isize> = TOASTS.lock().iter().map(|t| t.hwnd).collect();
    for h in hwnds {
        // SAFETY: hwnd from our own CreateWindow.
        let _ = unsafe { DestroyWindow(HWND(h as *mut _)) };
    }
    TOASTS.lock().clear();

    // Drop the cached font.
    let font = FONT_HANDLE.swap(0, Ordering::SeqCst);
    if font != 0 {
        // SAFETY: font was created via CreateFontW below.
        let _ = unsafe { DeleteObject(HGDIOBJ(font as *mut _)) };
    }

    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("toast thread exited");
}

/// Drain the caller-side `QUEUE` and spawn one HWND per request. Runs
/// only on the toast thread.
fn drain_queue(hinst: HINSTANCE, class_name_ptr: *const u16) {
    loop {
        let req = match QUEUE.lock().pop_front() {
            Some(r) => r,
            None => break,
        };
        spawn_toast(hinst, class_name_ptr, req);
    }
}

/// Apply a fresh monitor topology. Toasts on now-removed monitors are
/// pushed into early fade-out; alive toasts are repositioned because
/// surviving monitors can shift coordinates after an unplug.
fn apply_sync(specs: Vec<ToastSpec>) {
    let wanted: std::collections::HashSet<String> =
        specs.iter().map(|s| s.monitor_id.clone()).collect();

    // Force-dismiss toasts whose monitor vanished.
    {
        let now = Instant::now();
        let mut toasts = TOASTS.lock();
        for t in toasts.iter_mut() {
            if !wanted.contains(&t.monitor_id) && !t.forced_dismiss {
                t.forced_dismiss = true;
                t.fade_out_start = Some(now);
            }
        }
    }

    *MONITORS.lock() = specs;
    reflow_all_stacks();
}

/// Create a new toast HWND for `req`. If the target monitor is at
/// `max_concurrent`, force the oldest non-fading toast on that
/// monitor into fade-out first so the stack stays bounded.
fn spawn_toast(hinst: HINSTANCE, class_name_ptr: *const u16, req: ShowRequest) {
    let cfg = config_snapshot();
    if !cfg.enabled {
        return;
    }
    // Resolve the target monitor's work area; bail if we don't know
    // about it (e.g. the host queued a toast for a monitor that
    // unplugged before the bar thread woke up).
    let work_area = match MONITORS
        .lock()
        .iter()
        .find(|s| s.monitor_id == req.monitor_id)
        .map(|s| s.work_area)
    {
        Some(wa) => wa,
        None => {
            tracing::debug!(monitor = %req.monitor_id, "toast: target monitor unknown; dropping");
            return;
        }
    };

    // Eviction pass: count currently-non-dismissing toasts on this
    // monitor; if at cap, force-dismiss the oldest so the new one has
    // room. We do this *before* allocating the HWND so the stack
    // never visibly exceeds the cap.
    {
        let now = Instant::now();
        let mut toasts = TOASTS.lock();
        let mut on_monitor: Vec<&mut ToastWindow> = toasts
            .iter_mut()
            .filter(|t| t.monitor_id == req.monitor_id && !t.forced_dismiss)
            .collect();
        let cap = cfg.max_concurrent.max(1) as usize;
        if on_monitor.len() >= cap {
            // Sort oldest-first. `born` is monotonically increasing
            // per Instant semantics, so this is total.
            on_monitor.sort_by_key(|t| t.born);
            let evict_count = on_monitor.len() + 1 - cap;
            for t in on_monitor.into_iter().take(evict_count) {
                t.forced_dismiss = true;
                t.fade_out_start = Some(now);
            }
        }
    }

    // SAFETY: parameters are valid; class atom registered earlier.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(
                WS_EX_LAYERED.0
                    | WS_EX_TOPMOST.0
                    | WS_EX_TOOLWINDOW.0
                    | WS_EX_NOACTIVATE.0
                    | WS_EX_TRANSPARENT.0,
            ),
            PCWSTR(class_name_ptr),
            PCWSTR(class_name_ptr),
            WS_POPUP,
            // Initial position is overwritten by `reflow_stack` below.
            work_area.x,
            work_area.y,
            TOAST_W,
            TOAST_H,
            None,
            None,
            Some(hinst),
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, monitor = %req.monitor_id, "toast: CreateWindowExW failed");
            return;
        }
    };

    // Clip the window to a rounded-rect region so the corners are
    // actually rounded. Layered-window LWA_ALPHA gives the whole
    // window a uniform alpha (good for fade animations) but does NOT
    // round the corners — every paint pixel inside the client rect is
    // visible. SetWindowRgn tells the OS "ignore the corner triangles
    // entirely", which is cheaper than per-pixel alpha and matches
    // the focus_border overlay's approach.
    //
    // SAFETY: CreateRoundRectRgn is a pure GDI allocator; SetWindowRgn
    // assumes ownership of the HRGN on success, so we do NOT
    // DeleteObject the handle ourselves. bRedraw=TRUE forces an
    // immediate repaint with the new shape.
    let rgn = unsafe {
        CreateRoundRectRgn(0, 0, TOAST_W, TOAST_H, TOAST_RADIUS * 2, TOAST_RADIUS * 2)
    };
    if !rgn.is_invalid() {
        // SAFETY: hwnd from our own CreateWindow; rgn just allocated.
        let _ = unsafe { SetWindowRgn(hwnd, Some(rgn), true) };
    }

    // Start fully transparent so the fade-in is clean.
    // SAFETY: hwnd from our own CreateWindow; LWA_ALPHA is documented.
    let _ = unsafe {
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_ALPHA)
    };

    // Register this toast in the global list.
    {
        let mut toasts = TOASTS.lock();
        toasts.push(ToastWindow {
            hwnd: hwnd.0 as isize,
            monitor_id: req.monitor_id.clone(),
            level: req.level,
            text: req.text,
            born: Instant::now(),
            ttl: Duration::from_millis(cfg.ttl_ms as u64),
            forced_dismiss: false,
            fade_out_start: None,
            colors: cfg.colors,
        });
    }

    // Show without activating + start the animation timer. The timer
    // is auto-killed by `DestroyWindow`, so there's no explicit
    // cleanup path.
    // SAFETY: hwnd from our own CreateWindow.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    // SAFETY: hwnd valid; TIMERPROC=None routes to WM_TIMER.
    let _ = unsafe { SetTimer(Some(hwnd), ANIM_TIMER_ID, ANIM_INTERVAL_MS, None) };

    reflow_stack(&req.monitor_id);
}

/// Read a copy of the current config, falling back to defaults if the
/// host never called `start` (e.g. an in-process test exercising a
/// different code path that imports `toast`).
fn config_snapshot() -> ToastConfig {
    CONFIG
        .get()
        .map(|m| *m.lock())
        .unwrap_or_default()
}

/// Reposition the stack on a single monitor. Toasts are sorted by
/// `born` (oldest first) and laid out top-down at the configured
/// anchor.
fn reflow_stack(monitor_id: &str) {
    let work_area = match MONITORS
        .lock()
        .iter()
        .find(|s| s.monitor_id == monitor_id)
        .map(|s| s.work_area)
    {
        Some(wa) => wa,
        None => return,
    };
    let cfg = config_snapshot();

    // Snapshot the relevant toasts under the lock, then drop the lock
    // before issuing SetWindowPos so the wnd_proc can lock for paint
    // without deadlocking us. Order: oldest first so the stack visually
    // grows downward.
    let stack: Vec<isize> = {
        let toasts = TOASTS.lock();
        let mut filtered: Vec<(Instant, isize)> = toasts
            .iter()
            .filter(|t| t.monitor_id == monitor_id)
            .map(|t| (t.born, t.hwnd))
            .collect();
        filtered.sort_by_key(|(born, _)| *born);
        filtered.into_iter().map(|(_, h)| h).collect()
    };

    let (mut x, mut y) = match cfg.anchor {
        ToastAnchor::TopRight => (
            work_area.x + work_area.w - TOAST_W - TOAST_MARGIN,
            work_area.y + TOAST_MARGIN,
        ),
    };
    for hwnd in stack {
        // SAFETY: hwnd from our own CreateWindow; HWND_TOPMOST keeps
        // toasts above app windows; SWP_NOACTIVATE preserves focus.
        let _ = unsafe {
            SetWindowPos(
                HWND(hwnd as *mut _),
                Some(HWND_TOPMOST),
                x,
                y,
                TOAST_W,
                TOAST_H,
                SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0),
            )
        };
        match cfg.anchor {
            ToastAnchor::TopRight => {
                let _ = &mut x; // kept for symmetry with future anchors
                y += TOAST_H + TOAST_GAP;
            }
        }
    }
}

/// Reflow every monitor's stack. Used after a topology change.
fn reflow_all_stacks() {
    let monitor_ids: Vec<String> = MONITORS.lock().iter().map(|s| s.monitor_id.clone()).collect();
    for mid in monitor_ids {
        reflow_stack(&mid);
    }
}

// ---- wnd_proc + drawing ----------------------------------------------------

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER => {
            // Animation tick. Compute the right alpha and either apply
            // it or destroy the window.
            handle_timer(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            // SAFETY: hwnd is valid; PAINTSTRUCT is a valid out-param.
            unsafe { handle_paint(hwnd) };
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // We paint every pixel ourselves via WM_PAINT — suppress
            // the OS background erase so the corner pixels outside the
            // roundrect aren't filled with the (unset) class brush.
            LRESULT(1)
        }
        WM_DESTROY => LRESULT(0),
        // SAFETY: DefWindowProcW is the documented fallback.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[derive(Debug, Clone, Copy)]
enum AlphaState {
    Alpha(u8),
    Done,
}

fn handle_timer(hwnd: HWND) {
    // Compute the toast's current animation state under the lock,
    // then drop the lock before doing GDI work.
    let key = hwnd.0 as isize;
    let state = {
        let toasts = TOASTS.lock();
        toasts
            .iter()
            .find(|t| t.hwnd == key)
            .map(compute_alpha)
    };
    match state {
        Some(AlphaState::Alpha(a)) => {
            // SAFETY: hwnd is one of our own HWNDs; LWA_ALPHA is documented.
            let _ = unsafe {
                SetLayeredWindowAttributes(hwnd, COLORREF(0), a, LWA_ALPHA)
            };
        }
        Some(AlphaState::Done) => {
            // Remove from the global list FIRST so the WM_DESTROY
            // handler (synchronous) can't observe the toast still
            // present and double-process it.
            let monitor_id = {
                let mut toasts = TOASTS.lock();
                let mid = toasts
                    .iter()
                    .find(|t| t.hwnd == key)
                    .map(|t| t.monitor_id.clone());
                toasts.retain(|t| t.hwnd != key);
                mid
            };
            // SAFETY: hwnd from our own CreateWindow.
            let _ = unsafe { DestroyWindow(hwnd) };
            if let Some(mid) = monitor_id {
                reflow_stack(&mid);
            }
        }
        None => {
            // Toast vanished while we were locking — defensive no-op.
        }
    }
}

fn compute_alpha(t: &ToastWindow) -> AlphaState {
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(t.born);
    let fade_in = Duration::from_millis(FADE_IN_MS as u64);
    let fade_out = Duration::from_millis(FADE_OUT_MS as u64);

    // Compute `fade_out_began`: either the explicit override (set on
    // eviction) or `born + fade_in + ttl`.
    let natural_fade_start = t.born + fade_in + t.ttl;
    let fade_out_start = t.fade_out_start.unwrap_or(natural_fade_start);

    // If we've already entered fade-out (either naturally or via early
    // dismissal), that branch wins regardless of whether fade-in was
    // still in progress. Without this, evicting a toast during its
    // fade-in keeps it stuck at the rising-alpha state forever.
    if now >= fade_out_start {
        let into_fade_out = now.saturating_duration_since(fade_out_start);
        return if into_fade_out >= fade_out {
            AlphaState::Done
        } else {
            // Start the fade-out from the alpha we'd computed at the
            // moment fade_out_start was reached. For natural dismissal
            // that's 255; for an eviction during fade-in it's the
            // partial alpha we'd reached.
            let start_alpha = alpha_at(t, fade_out_start);
            let pct = (start_alpha as u32).saturating_sub(
                (into_fade_out.as_millis() as u32 * start_alpha as u32) / FADE_OUT_MS.max(1),
            );
            AlphaState::Alpha(pct.min(255) as u8)
        };
    }

    if now < t.born + fade_in {
        // Fade-in: 0..255 over FADE_IN_MS.
        let pct = elapsed.as_millis() as u32 * 255 / FADE_IN_MS.max(1);
        AlphaState::Alpha(pct.min(255) as u8)
    } else {
        AlphaState::Alpha(255)
    }
}

/// Helper for `compute_alpha` — what alpha would we have shown at
/// `instant`, ignoring any later fade-out? Used to seed the
/// fade-out from the right starting opacity when an eviction
/// interrupts fade-in.
fn alpha_at(t: &ToastWindow, instant: Instant) -> u8 {
    let fade_in = Duration::from_millis(FADE_IN_MS as u64);
    if instant < t.born + fade_in {
        let elapsed = instant.saturating_duration_since(t.born);
        let pct = elapsed.as_millis() as u32 * 255 / FADE_IN_MS.max(1);
        pct.min(255) as u8
    } else {
        255
    }
}

unsafe fn handle_paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: hwnd valid; ps is a valid out-param.
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if hdc.is_invalid() {
        return;
    }

    // Snapshot what we need under the lock, then release it for the
    // GDI work below.
    let key = hwnd.0 as isize;
    let snap: Option<(ToastLevel, String, ToastColors)> = {
        let toasts = TOASTS.lock();
        toasts
            .iter()
            .find(|t| t.hwnd == key)
            .map(|t| (t.level, t.text.clone(), t.colors))
    };
    let Some((level, text, colors)) = snap else {
        // SAFETY: hwnd valid; ps from BeginPaint.
        unsafe { let _ = EndPaint(hwnd, &ps); };
        return;
    };

    let (bg, fg) = match level {
        ToastLevel::Info => (colors.info_bg, colors.info_fg),
        ToastLevel::Warn => (colors.warn_bg, colors.warn_fg),
        ToastLevel::Error => (colors.error_bg, colors.error_fg),
    };

    // Fill the (rounded-clipped) client rect with the severity colour.
    // The window's `SetWindowRgn` clip applied at spawn time means
    // pixels outside the rounded shape are simply skipped by the OS,
    // so a single FillRect produces a real rounded pill.
    let client_rect = RECT {
        left: 0,
        top: 0,
        right: TOAST_W,
        bottom: TOAST_H,
    };
    // SAFETY: CreateSolidBrush is a pure GDI allocator; we DeleteObject
    // before returning. FillRect doesn't consume the brush.
    let bg_brush = unsafe { CreateSolidBrush(COLORREF(bg)) };
    if !bg_brush.is_invalid() {
        let _ = unsafe { FillRect(hdc, &client_rect, HBRUSH(bg_brush.0)) };
        let _ = unsafe { DeleteObject(HGDIOBJ(bg_brush.0)) };
    }

    // Select the cached font.
    let font = ensure_font();
    let old_font = unsafe { SelectObject(hdc, HGDIOBJ(font as *mut _)) };
    unsafe { SetBkMode(hdc, TRANSPARENT); }

    // Draw the leading severity glyph in a fixed left column.
    let glyph = match level {
        ToastLevel::Info => "\u{2713}",  // heavy check mark
        ToastLevel::Warn => "\u{26A0}",  // warning sign
        ToastLevel::Error => "\u{2715}", // ballot x
    };
    let glyph_rect = RECT {
        left: 12,
        top: 0,
        right: 36,
        bottom: TOAST_H,
    };
    unsafe { draw_text(hdc, glyph_rect, glyph, fg, DT_SINGLELINE | DT_VCENTER | DT_LEFT) };

    // Draw the message text in the remainder.
    let text_rect = RECT {
        left: 42,
        top: 0,
        right: TOAST_W - 12,
        bottom: TOAST_H,
    };
    unsafe {
        draw_text(
            hdc,
            text_rect,
            &text,
            fg,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS,
        )
    };

    unsafe { SelectObject(hdc, old_font); }
    // SAFETY: hwnd valid; ps from BeginPaint.
    let _ = unsafe { EndPaint(hwnd, &ps) };
}

/// Lazily allocate the shared font handle. Reused across every toast
/// paint to avoid `CreateFontW` churn.
fn ensure_font() -> isize {
    let cur = FONT_HANDLE.load(Ordering::SeqCst);
    if cur != 0 {
        return cur;
    }
    let mut face = utf16(b"Segoe UI Variable\0");
    // SAFETY: CreateFontW is a pure GDI allocator; null return is
    // handled implicitly because draw_text falls back to the default
    // GUI font when SelectObject(NULL) is a no-op.
    let font = unsafe {
        CreateFontW(
            16,                 // height
            0,                  // width (auto)
            0,
            0,
            FW_NORMAL.0 as i32, // weight
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
    // Try to publish; if another thread beat us, free our handle and
    // use theirs. This is a tight race; in practice the toast thread
    // is the only writer.
    if FONT_HANDLE
        .compare_exchange(0, raw, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        // SAFETY: handle came from CreateFontW above.
        let _ = unsafe { DeleteObject(HGDIOBJ(font.0 as *mut _)) };
        FONT_HANDLE.load(Ordering::SeqCst)
    } else {
        raw
    }
}

unsafe fn draw_text(
    hdc: HDC,
    rect: RECT,
    s: &str,
    color: u32,
    flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    // SAFETY: caller has selected a valid font into hdc.
    unsafe { SetTextColor(hdc, COLORREF(color)) };
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    let mut text_rc = rect;
    // SAFETY: hdc valid; wide and text_rc are valid out-params.
    unsafe {
        DrawTextW(hdc, &mut wide, &mut text_rc, flags);
    }
}

// ---- helpers ---------------------------------------------------------------

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}

#[inline]
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// ---- test helpers ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_toast(monitor_id: &str, born: Instant) -> ToastWindow {
        ToastWindow {
            hwnd: 0,
            monitor_id: monitor_id.to_string(),
            level: ToastLevel::Info,
            text: String::new(),
            born,
            ttl: Duration::from_millis(2000),
            forced_dismiss: false,
            fade_out_start: None,
            colors: ToastColors::default(),
        }
    }

    #[test]
    fn fade_in_ramps_alpha_from_zero() {
        // At t = born, alpha should be ~0.
        let t = fake_toast("m", Instant::now());
        match compute_alpha(&t) {
            AlphaState::Alpha(a) => assert!(a < 32, "expected near-zero alpha, got {a}"),
            AlphaState::Done => panic!("brand-new toast reported Done"),
        }
    }

    #[test]
    fn hold_phase_is_full_alpha() {
        // Simulate "born 200 ms ago" so we're past fade-in.
        let mut t = fake_toast("m", Instant::now() - Duration::from_millis(200));
        t.ttl = Duration::from_secs(60); // ensure we're not in fade-out
        match compute_alpha(&t) {
            AlphaState::Alpha(255) => {}
            other => panic!("hold phase expected Alpha(255), got {other:?}"),
        }
    }

    #[test]
    fn forced_dismiss_short_circuits_to_fade_out() {
        // Simulate eviction: born now, fade_out_start = now → we
        // immediately enter fade-out and finish after FADE_OUT_MS.
        let mut t = fake_toast("m", Instant::now());
        t.fade_out_start = Some(Instant::now() - Duration::from_millis(FADE_OUT_MS as u64 + 50));
        match compute_alpha(&t) {
            AlphaState::Done => {}
            other => panic!("expected Done after forced dismiss, got {other:?}"),
        }
    }

    #[test]
    fn default_config_round_trip() {
        let cfg = ToastConfig::default();
        assert!(cfg.enabled);
        assert!(cfg.ttl_ms > 0);
        assert!(cfg.max_concurrent >= 1);
    }
}
