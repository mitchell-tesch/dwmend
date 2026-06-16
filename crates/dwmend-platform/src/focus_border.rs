//! Focused-window highlight overlay.
//!
//! A single borderless top-most click-through window that draws a thick
//! rounded ring around the active tile. The ring is implemented as a
//! `SetWindowRgn` region that is the difference of two concentric rounded
//! rectangles — outer minus inner — so only the frame pixels exist in the
//! window. The window-class brush paints those pixels the configured colour.
//!
//! ## Why one window with a region (not 4 edge windows)?
//!
//! The previous implementation used 4 borderless edge popups (top/right/
//! bottom/left). That gave square corners that clashed with Windows 11's
//! ~8 px DWM rounding — the focus border was visibly sharper than the
//! window inside it, and the four corner triangles leaked the wallpaper
//! through. A single overlay clipped to a roundrect ring tracks the OS
//! corner radius cleanly.
//!
//! ## Threading
//!
//! One dedicated thread creates the window and runs a `GetMessage` pump so
//! the OS can deliver paint messages. The HWND is published via `OnceLock`
//! for the main thread to call `SetWindowPos` / `SetWindowRgn` /
//! `ShowWindow` on directly — those Win32 calls are explicitly cross-thread
//! safe.

use crate::Rect;
use crate::Result;
use color_eyre::eyre::eyre;
use crossbeam_channel::{Sender, bounded};
use parking_lot::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, HGDIOBJ,
    InvalidateRect, RGN_DIFF, SetWindowRgn,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GCLP_HBRBACKGROUND, GetMessageW,
    HWND_TOPMOST, IDC_ARROW, LoadCursorW, MSG, PostThreadMessageW, RegisterClassExW,
    SET_WINDOW_POS_FLAGS, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetClassLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_QUIT, WNDCLASSEXW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::PCWSTR;

/// Default corner radius — matches Windows 11's DWM-rounded windows.
pub const DEFAULT_RADIUS: i32 = 8;

static OVERLAY: OnceLock<isize> = OnceLock::new();
static BRUSH: Mutex<isize> = Mutex::new(0);
static WIDTH: AtomicI32 = AtomicI32::new(4);
static RADIUS: AtomicI32 = AtomicI32::new(DEFAULT_RADIUS);
static LISTENER_TID: AtomicIsize = AtomicIsize::new(0);
static VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Cached geometry of the most recently installed `SetWindowRgn` ring.
// Initialised to -1 so the first `place_overlay` call always rebuilds.
//
// Region rebuild is the dominant cost of a focus shift: 3 \u00d7 HRGN
// allocation + `CombineRgn(RGN_DIFF)` + `SetWindowRgn` (which itself
// invalidates and forces a repaint). When the user simply moves focus
// between two same-sized tiles \u2014 the most common case \u2014 the dimensions
// match and the previously-installed region is still correct, so all of
// that work is redundant. `SetWindowPos` already moves the window AND
// the region together (regions are window-relative), so a pure
// translate just needs the `SetWindowPos` on the parent function.
//
// The four atomics together form a single-slot cache key. We accept a
// rare double-rebuild on a TOCTOU race rather than serialising callers
// behind a Mutex \u2014 `place_overlay` runs on the main thread today, but
// even if a future feature called it concurrently the worst case is one
// extra rebuild, never an incorrect region.
static LAST_RGN_OUTER_W: AtomicI32 = AtomicI32::new(-1);
static LAST_RGN_OUTER_H: AtomicI32 = AtomicI32::new(-1);
static LAST_RGN_RING_W: AtomicI32 = AtomicI32::new(-1);
static LAST_RGN_RADIUS: AtomicI32 = AtomicI32::new(-1);

// ---- public API ------------------------------------------------------------

/// Spawn the border subsystem. `width` is the frame thickness in pixels,
/// `color` is a Win32 `COLORREF` (`0x00BBGGRR`), and `radius` is the inner
/// corner radius in pixels (8 matches Windows 11). Safe to call once.
pub fn start(width: i32, color: u32, radius: i32) -> Result<()> {
    WIDTH.store(width.max(1), Ordering::SeqCst);
    RADIUS.store(radius.max(0), Ordering::SeqCst);

    let (init_tx, init_rx) = bounded::<std::result::Result<isize, String>>(1);

    std::thread::Builder::new()
        .name("dwmend-border".into())
        .spawn(move || run_border_thread(color, init_tx))
        .map_err(|e| eyre!("spawn dwmend-border thread: {e}"))?;

    // Wait for the thread to finish creating the window so callers can show
    // it immediately.
    match init_rx
        .recv()
        .map_err(|_| eyre!("border thread died during init"))?
    {
        Ok(hwnd_isize) => {
            let _ = OVERLAY.set(hwnd_isize);
            tracing::info!(
                width,
                radius,
                color = format!("{color:#x}"),
                "focus border initialised"
            );
            Ok(())
        }
        Err(e) => Err(eyre!("focus border init failed: {e}")),
    }
}

/// Reposition and show the border around `target`. `target` is in screen
/// coordinates — typically the focused tile's rect.
pub fn show_around(target: Rect) {
    let Some(&h) = OVERLAY.get() else { return };
    let hwnd = HWND(h as *mut _);
    let w = WIDTH.load(Ordering::SeqCst).max(1);
    let r = RADIUS.load(Ordering::SeqCst).max(0);
    place_overlay(hwnd, target, w, r);
    if !VISIBLE.swap(true, Ordering::SeqCst) {
        // Coming back from hidden — make sure SW_SHOWNOACTIVATE goes through.
        // SAFETY: HWND from our own CreateWindow.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    }
}

/// Hide the border entirely (no focused window, or paused).
pub fn hide() {
    if !VISIBLE.swap(false, Ordering::SeqCst) {
        return;
    }
    let Some(&h) = OVERLAY.get() else { return };
    // SAFETY: HWND from our own CreateWindow.
    let _ = unsafe { ShowWindow(HWND(h as *mut _), SW_HIDE) };
}

/// Swap the border color at runtime (config reload). Replaces the shared
/// `hbrBackground` brush; the next paint cycle will use the new color.
pub fn set_color(color: u32) {
    let Some(&h) = OVERLAY.get() else { return };
    let hwnd = HWND(h as *mut _);
    // SAFETY: CreateSolidBrush is thread-safe; HBRUSH is opaque.
    let new_brush = unsafe { CreateSolidBrush(COLORREF(color)) };
    if new_brush.is_invalid() {
        return;
    }
    let mut guard = BRUSH.lock();
    let old = std::mem::replace(&mut *guard, new_brush.0 as isize);
    drop(guard);
    // Repoint the class' background brush; the next paint uses it.
    // SAFETY: hwnd valid; class index is well-known.
    let _ = unsafe { SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, new_brush.0 as isize) };
    // SAFETY: hwnd valid; bErase = TRUE so the brush is used immediately.
    let _ = unsafe { InvalidateRect(Some(hwnd), None, true) };
    if old != 0 {
        // SAFETY: the brush we're freeing was created by CreateSolidBrush.
        let _ = unsafe { DeleteObject(HGDIOBJ(old as *mut _)) };
    }
}

/// Update the frame thickness. Takes effect on the next `show_around`.
pub fn set_width(width: i32) {
    WIDTH.store(width.max(1), Ordering::SeqCst);
}

/// Update the inner corner radius. Takes effect on the next `show_around`.
pub fn set_radius(radius: i32) {
    RADIUS.store(radius.max(0), Ordering::SeqCst);
}

/// Cooperative shutdown — post WM_QUIT to the pump.
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

// ---- listener thread -------------------------------------------------------

fn run_border_thread(initial_color: u32, init_tx: Sender<std::result::Result<isize, String>>) {
    // SAFETY: GetCurrentThreadId is always safe.
    let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() } as isize;
    LISTENER_TID.store(tid, Ordering::SeqCst);

    let hwnd = match create_window(initial_color) {
        Ok(h) => h,
        Err(msg) => {
            let _ = init_tx.send(Err(msg));
            return;
        }
    };
    let _ = init_tx.send(Ok(hwnd.0 as isize));
    tracing::info!(tid = tid, "focus border thread started");

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-param; hwnd=None pumps every thread message.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        // SAFETY: msg is populated.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Drop the brush.
    let brush = *BRUSH.lock();
    if brush != 0 {
        // SAFETY: brush was created via CreateSolidBrush.
        let _ = unsafe { DeleteObject(HGDIOBJ(brush as *mut _)) };
    }
    LISTENER_TID.store(0, Ordering::SeqCst);
    tracing::info!("focus border thread exited");
}

fn create_window(initial_color: u32) -> std::result::Result<HWND, String> {
    let class_name = utf16(b"DwmendFocusBorder\0");

    // SAFETY: GetModuleHandleW(None) returns the current EXE.
    let hinst = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HINSTANCE(h.0),
        Err(e) => return Err(format!("GetModuleHandleW: {e}")),
    };

    // SAFETY: CreateSolidBrush is thread-safe.
    let brush = unsafe { CreateSolidBrush(COLORREF(initial_color)) };
    if brush.is_invalid() {
        return Err("CreateSolidBrush failed".into());
    }
    *BRUSH.lock() = brush.0 as isize;

    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hbrBackground: brush,
        // Without an explicit hCursor the OS falls back to IDC_APPSTARTING
        // (the busy/spinner cursor) when the pointer hovers over the
        // border — even though WS_EX_TRANSPARENT makes us click-through.
        // Load the standard arrow so the cursor stays normal.
        // SAFETY: LoadCursorW with HINSTANCE=NULL and IDC_ARROW returns a
        // shared system cursor handle owned by the OS — we never free it.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        ..Default::default()
    };
    // SAFETY: class is fully initialised; class_name is null-terminated wstr.
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        return Err("RegisterClassExW failed".into());
    }

    let ex = WINDOW_EX_STYLE(
        WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0 | WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0,
    );

    // SAFETY: parameters are valid; PCWSTR pointers outlive the call.
    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None, // hWndParent
            None, // hMenu
            Some(hinst),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW: {e}"))?;

    Ok(hwnd)
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The class brush + the SetWindowRgn clip do all the rendering; we have
    // no custom painting. DefWindowProcW for every message is correct.
    // SAFETY: DefWindowProcW is the documented fallback handler.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Reposition the overlay so its outer bounds hug `target` with `w` pixels
/// of frame around it, then rebuild its region to be a roundrect ring with
/// inner corner radius `inner_r`. The outer radius is `inner_r + w` so the
/// inner and outer curves are concentric and the frame thickness is uniform
/// around the corner.
///
/// The region rebuild step is skipped when `(outer_w, outer_h, w, inner_r)`
/// match the most recently installed region \u2014 i.e. the user moved focus
/// between two tiles of identical dimensions. Regions are window-relative,
/// so `SetWindowPos` carries the existing region with the window for free
/// in that case.
fn place_overlay(hwnd: HWND, target: Rect, w: i32, inner_r: i32) {
    let outer_x = target.x - w;
    let outer_y = target.y - w;
    let outer_w = target.w + 2 * w;
    let outer_h = target.h + 2 * w;

    // 1. Move + re-assert TOPMOST so a focus change on the underlying app
    //    can't bury us. SWP_NOACTIVATE keeps the foreground stable.
    let flags = SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0);
    // SAFETY: HWND from our own CreateWindow; values are valid.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            outer_x,
            outer_y,
            outer_w,
            outer_h,
            flags,
        )
    };

    // 2. Cache check: skip region rebuild when geometry is unchanged.
    //    Each `swap` returns the previous value; an all-match outcome
    //    means the currently-installed region is still correct for this
    //    target rect and we can return without any GDI region work.
    let prev_outer_w = LAST_RGN_OUTER_W.swap(outer_w, Ordering::Relaxed);
    let prev_outer_h = LAST_RGN_OUTER_H.swap(outer_h, Ordering::Relaxed);
    let prev_ring_w = LAST_RGN_RING_W.swap(w, Ordering::Relaxed);
    let prev_radius = LAST_RGN_RADIUS.swap(inner_r, Ordering::Relaxed);
    if prev_outer_w == outer_w
        && prev_outer_h == outer_h
        && prev_ring_w == w
        && prev_radius == inner_r
    {
        return;
    }

    // 3. Build the ring region.
    //    SetWindowRgn coordinates are window-relative (top-left = 0,0).
    //    `CreateRoundRectRgn(x1,y1,x2,y2,wEllipse,hEllipse)`:
    //      - The rect is [x1, y1, x2, y2) \u2014 x2/y2 are exclusive.
    //      - wEllipse / hEllipse are the *diameters* of the corner ellipse,
    //        i.e. 2 * radius.
    let outer_r = (inner_r + w).max(0);
    // SAFETY: GDI region creators are pure-data; failure returns a null HRGN
    // that downstream calls treat as a no-op.
    let outer_rgn = unsafe { CreateRoundRectRgn(0, 0, outer_w, outer_h, outer_r * 2, outer_r * 2) };
    let inner_rgn =
        unsafe { CreateRoundRectRgn(w, w, w + target.w, w + target.h, inner_r * 2, inner_r * 2) };
    // SAFETY: CreateRectRgn(0,0,0,0) yields an empty destination we fill via
    // CombineRgn below.
    let ring = unsafe { CreateRectRgn(0, 0, 0, 0) };
    // SAFETY: all three HRGNs are valid (or null, treated as empty).
    let _ = unsafe { CombineRgn(Some(ring), Some(outer_rgn), Some(inner_rgn), RGN_DIFF) };
    // After CombineRgn the srcs are no longer needed.
    // SAFETY: HRGN is a HGDIOBJ subtype; DeleteObject accepts a null HRGN.
    let _ = unsafe { DeleteObject(HGDIOBJ(outer_rgn.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(inner_rgn.0)) };
    // 4. Hand `ring` to the OS \u2014 it owns the HRGN now and frees the old one.
    // SAFETY: HWND from our own CreateWindow; ring is valid; bredraw=TRUE.
    let _ = unsafe { SetWindowRgn(hwnd, Some(ring), true) };
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes.iter().map(|&b| b as u16).collect()
}
