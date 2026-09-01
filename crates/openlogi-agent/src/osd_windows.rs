//! The crown mode indicator: a segmented bar naming every mode with the
//! active one lit, shown for a moment after a tap cycles.
//!
//! Styled after Logitech's own Craft crown overlay — a light, frosted strip of
//! icon-over-label cells at the bottom of the work area.
//!
//! Raw Win32 rather than a GPUI window, for one decisive reason: a HUD must be
//! **click-through**. `WS_EX_TRANSPARENT` gives that, and GPUI (whose revision
//! is pinned by `Cargo.lock`) exposes no passthrough option, so an overlay
//! window would swallow every click inside its bounds for as long as it showed.
//! `WS_EX_NOACTIVATE` plus `SW_SHOWNOACTIVATE` keeps it from ever taking focus,
//! so it cannot interrupt typing. Living in the agent also means no IPC hop
//! between the crown event and the pixels.
//!
//! Runs its own message pump on a dedicated thread; the agent sets the view
//! state and pokes the window with a private message.

#![expect(
    unsafe_code,
    reason = "raw win32: a layered click-through popup and its message pump — localized here"
)]

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use openlogi_agent_core::CrownModeIndicator;
use tracing::{debug, warn};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DT_CENTER,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, DeleteObject, DrawTextW, EndPaint,
    FF_DONTCARE, FW_NORMAL, FillRect, HDC, HFONT, InvalidateRect, PAINTSTRUCT, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_TOPMOST, KillTimer, MSG,
    PostMessageW, RegisterClassW, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, SystemParametersInfoW,
    TranslateMessage, WM_APP, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Window class name, also how a test or a support session can find the HUD.
pub const CLASS_NAME: &str = "OpenLogiCrownOsd";

/// Private message telling the pump the view changed.
const WM_REFRESH: u32 = WM_APP + 1;
const HIDE_TIMER: usize = 1;
/// How long the bar stays up after a tap. Long enough to read three cells,
/// short enough not to linger over what you were doing.
const HIDE_MS: u32 = 1600;
const LWA_ALPHA: u32 = 0x02;
/// Whole-window opacity; below 255 the desktop shows faintly through.
const ALPHA: u8 = 232;
/// `DWMWA_WINDOW_CORNER_PREFERENCE`, and `DWMWCP_ROUND` — Windows 11 rounds
/// the corners, older builds ignore the call.
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2;

// Light palette matching Logitech's overlay. Deliberately not the app theme:
// this is a hardware affordance, and it reads the same over any window.
const CELL_INACTIVE: COLORREF = rgb(206, 207, 210);
const CELL_ACTIVE: COLORREF = rgb(247, 247, 248);
const SEPARATOR: COLORREF = rgb(188, 189, 193);
const INK: COLORREF = rgb(45, 45, 50);
const INK_DIM: COLORREF = rgb(96, 98, 104);

/// Cell metrics in logical pixels, scaled by the window's DPI at paint time.
const CELL_W: f32 = 138.0;
const CELL_H: f32 = 92.0;
/// Gap between the bar and the bottom of the work area.
const BOTTOM_MARGIN: f32 = 64.0;

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// One mode's cell: a glyph over its name.
#[derive(Debug, Clone)]
struct Cell {
    glyph: char,
    label: String,
}

/// What the window should draw.
#[derive(Debug, Clone, Default)]
enum View {
    #[default]
    Hidden,
    Bar {
        cells: Vec<Cell>,
        selected: usize,
    },
}

/// Shared view state: written by the agent, read by the pump thread.
type SharedView = Arc<Mutex<View>>;

/// Handle for showing the indicator. Cloneable and inert until the pump
/// thread has its window, so an early mode cycle is dropped rather than
/// blocking.
#[derive(Clone)]
pub struct Osd {
    /// The pump's window, or 0 before it exists. `isize` because a raw `HWND`
    /// is not `Send`; only the pump thread ever dereferences it.
    hwnd: Arc<AtomicIsize>,
    view: SharedView,
}

impl Osd {
    /// Show the mode bar for [`HIDE_MS`], replacing whatever is up.
    pub fn show(&self, indicator: &CrownModeIndicator) {
        let cells = indicator
            .names
            .iter()
            .map(|name| Cell {
                glyph: glyph_for(name),
                label: name.clone(),
            })
            .collect();
        self.set(View::Bar {
            cells,
            selected: indicator.selected,
        });
    }

    fn set(&self, next: View) {
        // Recover from a poisoned lock: the critical section is a plain
        // assignment, so the data cannot be half-written.
        *self.view.lock().unwrap_or_else(PoisonError::into_inner) = next;
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd == 0 {
            debug!("crown OSD not up yet — mode change not shown");
            return;
        }
        // SAFETY: `hwnd` was published by the pump thread after a successful
        // `CreateWindowExW` and is destroyed only when that thread ends with
        // the process. `PostMessageW` is explicitly safe across threads.
        unsafe {
            PostMessageW(hwnd as HWND, WM_REFRESH, 0, 0);
        }
    }
}

/// Start the indicator's window on its own thread.
///
/// Returns a handle even when the thread cannot start: a missing HUD must not
/// stop the agent, and the handle is inert while its window is absent.
pub fn spawn() -> Osd {
    let osd = Osd {
        hwnd: Arc::new(AtomicIsize::new(0)),
        view: Arc::new(Mutex::new(View::Hidden)),
    };
    let hwnd = Arc::clone(&osd.hwnd);
    let view = Arc::clone(&osd.view);
    if let Err(error) = std::thread::Builder::new()
        .name("crown-osd".into())
        .spawn(move || {
            // SAFETY: every call inside is a window-manager call made from the
            // thread that owns the window, which is what Win32 requires.
            unsafe { pump(&hwnd, &view) }
        })
    {
        warn!(%error, "could not start the crown OSD thread — mode changes stay silent");
    }
    osd
}

/// A Segoe MDL2 glyph for a mode name, falling back to a neutral badge.
///
/// Matched on the name because a mode is user-named: someone calling a mode
/// "Zoom" gets the zoom glyph without configuring anything, and an unmatched
/// name still gets a cell rather than an empty box.
fn glyph_for(label: &str) -> char {
    match label.to_lowercase().as_str() {
        "scroll" => '\u{E8CB}',
        "tabs" | "windows" => '\u{E71D}',
        "zoom" => '\u{E8A3}',
        "volume" | "sound" => '\u{E767}',
        "media" | "playback" => '\u{E768}',
        "keys" | "shortcuts" => '\u{E765}',
        "brightness" => '\u{E706}',
        "undo" | "history" => '\u{E7A7}',
        _ => '\u{E946}',
    }
}

/// Create the window and run its message loop until the process ends.
unsafe fn pump(shared: &Arc<AtomicIsize>, view: &SharedView) {
    let class_name = wide(CLASS_NAME);
    // SAFETY: null module handle asks for the current process's, as documented.
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class_name.as_ptr(),
    };
    // SAFETY: `class` is fully initialized and its name buffer outlives the
    // call; the window procedure is a real `extern "system"` fn.
    if unsafe { RegisterClassW(&raw const class) } == 0 {
        warn!("crown OSD: RegisterClassW failed");
        return;
    }
    let title = wide("OpenLogi Crown");
    // SAFETY: both wide strings are NUL-terminated and outlive the call. The
    // ex-styles are what make this click-through and non-activating.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        warn!("crown OSD: CreateWindowExW failed");
        return;
    }
    // SAFETY: `hwnd` is a live window this thread owns.
    unsafe {
        SetLayeredWindowAttributes(hwnd, 0, ALPHA, LWA_ALPHA);
        let round = DWMWCP_ROUND;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const round).cast(),
            u32::try_from(size_of::<u32>()).unwrap_or(4),
        );
    }
    set_view_for(hwnd, view);
    shared.store(hwnd as isize, Ordering::Release);
    debug!("crown OSD ready");

    // SAFETY: `MSG` is a plain C struct for which all-zero is a valid value.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: `msg` is a valid out-parameter for the lifetime of the loop and
    // the window belongs to this thread.
    while unsafe { GetMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: `msg` was just filled by `GetMessageW`; dispatching it is
        // what the pump exists to do.
        unsafe {
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

// The pump thread's copy of the shared view, so `wndproc` can reach it.
// A thread-local rather than a global: only the pump thread's window procedure
// reads it, and it is set before the window can receive a message.
thread_local! {
    static VIEW: std::cell::RefCell<Option<SharedView>> = const { std::cell::RefCell::new(None) };
}

fn set_view_for(_hwnd: HWND, view: &SharedView) {
    VIEW.with_borrow_mut(|slot| *slot = Some(Arc::clone(view)));
}

fn current_view() -> View {
    VIEW.with_borrow(|slot| {
        slot.as_ref().map_or(View::Hidden, |view| {
            view.lock().unwrap_or_else(PoisonError::into_inner).clone()
        })
    })
}

fn clear_view() {
    VIEW.with_borrow(|slot| {
        if let Some(view) = slot.as_ref() {
            *view.lock().unwrap_or_else(PoisonError::into_inner) = View::Hidden;
        }
    });
}

/// The window's size in physical pixels, or `None` when nothing should show.
fn layout(scale: f32, view: &View) -> Option<(i32, i32)> {
    match view {
        View::Hidden => None,
        View::Bar { cells, .. } => {
            let count = u16::try_from(cells.len()).ok()?;
            if count == 0 {
                return None;
            }
            Some((px(f32::from(count) * CELL_W, scale), px(CELL_H, scale)))
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_REFRESH => {
            let view = current_view();
            // SAFETY: `hwnd` is this thread's window for every call below.
            unsafe {
                KillTimer(hwnd, HIDE_TIMER);
                match layout(scale(hwnd), &view) {
                    None => {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                    Some((w, h)) => {
                        let (x, y) = position(hwnd, w, h);
                        SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        InvalidateRect(hwnd, std::ptr::null(), 1);
                        SetTimer(hwnd, HIDE_TIMER, HIDE_MS, None);
                    }
                }
            }
            0
        }
        WM_TIMER if wparam == HIDE_TIMER => {
            clear_view();
            // SAFETY: this thread's window.
            unsafe {
                KillTimer(hwnd, HIDE_TIMER);
                ShowWindow(hwnd, SW_HIDE);
            }
            0
        }
        WM_PAINT => {
            let view = current_view();
            let scale = scale(hwnd);
            // SAFETY: `ps` is a valid out-parameter and every GDI handle
            // created in `paint` is released there; `BeginPaint`/`EndPaint`
            // are correctly paired.
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &raw mut ps);
                paint(hdc, ps.rcPaint, &view, scale);
                EndPaint(hwnd, &raw const ps);
            }
            0
        }
        // SAFETY: forwarding untouched arguments to the default handler.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Bottom-centre of the primary monitor's work area, so the bar clears the
/// taskbar instead of hiding under it.
fn position(hwnd: HWND, w: i32, h: i32) -> (i32, i32) {
    // SAFETY: `work` is a valid out-parameter sized for `SPI_GETWORKAREA`.
    let mut work: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: `work` is a `RECT`, the out-parameter shape `SPI_GETWORKAREA`
    // documents, and it outlives the call.
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work).cast(), 0) };
    if ok == 0 {
        return (0, 0);
    }
    let x = work.left + (work.right - work.left - w) / 2;
    let y = work.bottom - h - px(BOTTOM_MARGIN, scale(hwnd));
    (x, y)
}

fn scale(hwnd: HWND) -> f32 {
    // SAFETY: `hwnd` is a live window; the call returns 0 on failure.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        1.0
    } else {
        #[expect(
            clippy::cast_precision_loss,
            reason = "DPI values are small integers; f32 represents them exactly"
        )]
        let dpi = dpi as f32;
        dpi / 96.0
    }
}

/// Logical pixels to physical, clamped into `i32` — window metrics never
/// legitimately exceed it, and a saturating cast beats a wrapped negative size.
fn px(logical: f32, scale: f32) -> i32 {
    let scaled = logical * scale;
    if scaled.is_finite() {
        // `as` on a float saturates at the integer bounds in Rust.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "truncating to whole pixels is the intent; the cast saturates"
        )]
        let rounded = scaled.round() as i32;
        rounded
    } else {
        0
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn make_font(size_px: i32, weight: i32, face: &str) -> HFONT {
    let face = wide(face);
    // SAFETY: `face` is NUL-terminated and outlives the call.
    unsafe {
        CreateFontW(
            -size_px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            u32::from(DEFAULT_CHARSET),
            0,
            0,
            u32::from(CLEARTYPE_QUALITY),
            u32::from(FF_DONTCARE) << 4,
            face.as_ptr(),
        )
    }
}

unsafe fn paint(hdc: HDC, rc: RECT, view: &View, scale: f32) {
    // SAFETY: `hdc` is the DC `BeginPaint` handed us; every object created
    // here is deleted before returning.
    unsafe {
        let base = CreateSolidBrush(CELL_INACTIVE);
        FillRect(hdc, &raw const rc, base);
        DeleteObject(base.cast());
        SetBkMode(hdc, TRANSPARENT.cast_signed());
    }

    let View::Bar { cells, selected } = view else {
        return;
    };

    // SAFETY: as above — fonts are selected out and deleted below.
    unsafe {
        let icon_font = make_font(
            px(30.0, scale),
            FW_NORMAL.cast_signed(),
            "Segoe MDL2 Assets",
        );
        let label_font = make_font(px(15.0, scale), FW_NORMAL.cast_signed(), "Segoe UI");
        let cw = px(CELL_W, scale);
        let ch = px(CELL_H, scale);

        for (i, cell) in cells.iter().enumerate() {
            let Ok(index) = i32::try_from(i) else {
                break;
            };
            let left = rc.left + index * cw;
            let cell_rc = RECT {
                left,
                top: rc.top,
                right: left + cw,
                bottom: rc.top + ch,
            };
            let active = i == *selected;

            let fill = CreateSolidBrush(if active { CELL_ACTIVE } else { CELL_INACTIVE });
            FillRect(hdc, &raw const cell_rc, fill);
            DeleteObject(fill.cast());

            // Hairline between cells, but not after the last one.
            if i + 1 < cells.len() {
                let sep = CreateSolidBrush(SEPARATOR);
                let line = RECT {
                    left: cell_rc.right - px(1.0, scale).max(1),
                    top: cell_rc.top + px(14.0, scale),
                    right: cell_rc.right,
                    bottom: cell_rc.bottom - px(14.0, scale),
                };
                FillRect(hdc, &raw const line, sep);
                DeleteObject(sep.cast());
            }

            let icon_rc = RECT {
                left: cell_rc.left,
                top: cell_rc.top + px(16.0, scale),
                right: cell_rc.right,
                bottom: cell_rc.top + px(56.0, scale),
            };
            SelectObject(hdc, icon_font.cast());
            SetTextColor(hdc, INK);
            draw_text(
                hdc,
                &cell.glyph.to_string(),
                icon_rc,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );

            let label_rc = RECT {
                left: cell_rc.left + px(6.0, scale),
                top: cell_rc.bottom - px(30.0, scale),
                right: cell_rc.right - px(6.0, scale),
                bottom: cell_rc.bottom - px(8.0, scale),
            };
            SelectObject(hdc, label_font.cast());
            SetTextColor(hdc, if active { INK } else { INK_DIM });
            draw_text(hdc, &cell.label, label_rc, DT_CENTER | DT_WORDBREAK);
        }
        DeleteObject(icon_font.cast());
        DeleteObject(label_font.cast());
    }
}

unsafe fn draw_text(hdc: HDC, text: &str, mut rect: RECT, flags: u32) {
    let buf = wide(text);
    let Ok(len) = i32::try_from(buf.len().saturating_sub(1)) else {
        return;
    };
    // SAFETY: `buf` outlives the call and `len` is its length without the NUL,
    // which is what `DrawTextW` expects for a counted string.
    unsafe {
        DrawTextW(hdc, buf.as_ptr(), len, &raw mut rect, flags | DT_NOPREFIX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hidden_view_has_no_window_size() {
        assert_eq!(layout(1.0, &View::Hidden), None);
    }

    #[test]
    fn an_empty_bar_has_no_window_size() {
        assert_eq!(
            layout(
                1.0,
                &View::Bar {
                    cells: Vec::new(),
                    selected: 0,
                }
            ),
            None
        );
    }

    #[test]
    fn the_bar_grows_one_cell_per_mode() {
        let cells = vec![
            Cell {
                glyph: 'a',
                label: "Volume".into(),
            },
            Cell {
                glyph: 'b',
                label: "Scroll".into(),
            },
        ];
        let (w, h) = layout(1.0, &View::Bar { cells, selected: 0 }).expect("two cells");
        assert_eq!(w, px(CELL_W * 2.0, 1.0));
        assert_eq!(h, px(CELL_H, 1.0));
    }

    #[test]
    fn scaling_widens_the_bar() {
        let cells = vec![Cell {
            glyph: 'a',
            label: "Volume".into(),
        }];
        let (single, _) = layout(
            1.0,
            &View::Bar {
                cells: cells.clone(),
                selected: 0,
            },
        )
        .expect("one cell");
        let (double, _) = layout(2.0, &View::Bar { cells, selected: 0 }).expect("one cell at 2x");
        assert_eq!(double, single * 2);
    }

    /// A non-finite scale must collapse to nothing rather than to a giant
    /// window: a zero size leaves the bar hidden, where a saturated one would
    /// paint over the whole screen.
    #[test]
    fn a_nonsense_scale_collapses_to_zero() {
        assert_eq!(px(100.0, f32::NAN), 0);
        assert_eq!(px(100.0, f32::INFINITY), 0);
        assert_eq!(px(100.0, f32::NEG_INFINITY), 0);
    }

    /// A negative scale would otherwise flip the window inside out.
    #[test]
    fn a_negative_scale_never_yields_a_negative_size() {
        assert!(px(100.0, -1.0) <= 0, "a negative size must not be shown");
    }

    #[test]
    fn known_mode_names_get_their_own_glyph() {
        assert_ne!(glyph_for("Volume"), glyph_for("Scroll"));
        assert_eq!(glyph_for("volume"), glyph_for("Volume"), "case-insensitive");
    }

    /// An unnamed-in-the-table mode still gets a cell rather than a blank.
    #[test]
    fn an_unknown_mode_name_falls_back_to_a_badge() {
        assert_eq!(glyph_for("Whatever The User Typed"), '\u{E946}');
    }
}
