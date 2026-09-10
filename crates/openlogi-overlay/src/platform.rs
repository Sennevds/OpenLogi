//! Native window policy for the standalone Actions Ring overlay.

/// Keep the overlay out of the Dock and app switcher.
#[cfg(target_os = "macos")]
pub fn configure_application() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    if let Some(marker) = MainThreadMarker::new() {
        NSApplication::sharedApplication(marker)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

/// Make the transparent ring panel borderless and remove its native shadow.
#[cfg(target_os = "macos")]
pub fn configure_windows() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSWindowStyleMask};

    if let Some(marker) = MainThreadMarker::new() {
        for window in NSApplication::sharedApplication(marker).windows() {
            window.setStyleMask(NSWindowStyleMask::NonactivatingPanel);
            window.setHasShadow(false);
        }
    }
}

/// No native application policy is required away from macOS.
#[cfg(not(target_os = "macos"))]
pub fn configure_application() {}

/// Make `window` genuinely transparent where it paints nothing.
///
/// GPUI's Windows renderer already clears a non-opaque window to `[0,0,0,0]`
/// on a premultiplied DirectComposition swapchain, so per-pixel transparency is
/// *available*. What defeats it is the DWM accent policy GPUI then applies to
/// the same window: `Transparent` maps to accent state 2 (a whitish veil) and
/// `Blurred` to state 4 (acrylic), and either one paints the entire window
/// rectangle — the square around the ring. A `SetWindowRgn` region does not
/// help, because DWM composites DirectComposition content without regard to it;
/// that was tried, and the log said "clipped" while the square stayed.
///
/// So this re-issues the same undocumented call GPUI uses, with the accent
/// state set to *disabled*. The clear colour is untouched, the swapchain is
/// untouched, and with no accent policy the unpainted pixels are simply not
/// there. Undocumented, so it is resolved at runtime and a miss is logged, not
/// assumed.
#[cfg(target_os = "windows")]
pub fn disable_backdrop(window: &gpui::Window) {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    /// `WCA_ACCENT_POLICY`, mirroring GPUI's own use of the same private API.
    const WCA_ACCENT_POLICY: u32 = 0x13;
    /// `ACCENT_DISABLED`: no veil, no acrylic, nothing painted behind the scene.
    const ACCENT_DISABLED: u32 = 0;

    #[repr(C)]
    struct AccentPolicy {
        accent_state: u32,
        accent_flags: u32,
        gradient_color: u32,
        animation_id: u32,
    }

    #[repr(C)]
    struct WindowCompositionAttribData {
        attrib: u32,
        pv_data: *mut c_void,
        cb_data: usize,
    }

    type SetWindowCompositionAttribute =
        unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> i32;

    let Some(hwnd) = hwnd_of(window) else {
        return;
    };

    #[expect(
        unsafe_code,
        reason = "SetWindowCompositionAttribute is an undocumented user32 export with no safe binding; GPUI reaches it the same way"
    )]
    // SAFETY: both strings are NUL-terminated literals; `GetModuleHandleA` on a
    // module this process has already loaded (GPUI's renderer uses user32)
    // returns a non-owning handle, and `GetProcAddress` on it is a pure lookup.
    let entry = unsafe {
        let user32 = GetModuleHandleA(c"user32.dll".as_ptr().cast());
        if user32.is_null() {
            None
        } else {
            GetProcAddress(user32, c"SetWindowCompositionAttribute".as_ptr().cast())
        }
    };
    let Some(entry) = entry else {
        tracing::warn!("SetWindowCompositionAttribute is unavailable — backdrop stays");
        return;
    };

    let accent = AccentPolicy {
        accent_state: ACCENT_DISABLED,
        accent_flags: 0,
        gradient_color: 0,
        animation_id: 0,
    };
    let mut data = WindowCompositionAttribData {
        attrib: WCA_ACCENT_POLICY,
        pv_data: (&raw const accent).cast_mut().cast(),
        cb_data: std::mem::size_of::<AccentPolicy>(),
    };
    #[expect(
        unsafe_code,
        reason = "see above — calling the resolved private entry point"
    )]
    // SAFETY: `entry` is the address user32 reported for this exact export, and
    // its signature is the one Windows has used since Windows 10 1809 (GPUI
    // relies on the same). `hwnd` is live, and `data` points at a struct that
    // outlives the call.
    let applied = unsafe {
        let set: SetWindowCompositionAttribute = std::mem::transmute(entry);
        set(hwnd, &raw mut data)
    } != 0;
    if applied {
        tracing::debug!("ring window backdrop disabled — per-pixel transparency in effect");
    } else {
        tracing::warn!("SetWindowCompositionAttribute refused — backdrop stays");
    }

    remove_dwm_frame(hwnd);
}

/// The Win32 handle behind a GPUI window, or `None` (logged) when there is none.
#[cfg(target_os = "windows")]
fn hwnd_of(window: &gpui::Window) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;

    // The trait method by full path: GPUI's inherent `Window::window_handle`
    // returns its own `AnyWindowHandle` and would shadow this one.
    let handle = match HasWindowHandle::window_handle(window) {
        Ok(handle) => handle.as_raw(),
        Err(error) => {
            tracing::warn!(%error, "ring window exposes no native handle — backdrop stays");
            return None;
        }
    };
    let RawWindowHandle::Win32(win32) = handle else {
        tracing::warn!("ring window is not a Win32 window — backdrop stays");
        return None;
    };
    Some(win32.hwnd.get() as HWND)
}

/// Strip Windows 11's own frame from `hwnd`.
///
/// With the backdrop gone, what is left of the rectangle is drawn on every
/// top-level window unless it opts out: rounded corners, a 1px border, and —
/// outliving `BORDER_COLOR = NONE` — a 1px white non-client edge along the top.
/// The macOS arm opts out of the equivalent with `setHasShadow(false)`; these
/// are the documented DWM knobs.
#[cfg(target_os = "windows")]
fn remove_dwm_frame(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::Graphics::Dwm::{
        DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
    };

    /// Every attribute here is a `u32`, and DWM wants its byte size — written
    /// as a const expression so no cast can truncate it.
    const ATTRIBUTE_SIZE: u32 = u32::BITS / 8;

    let frame: [(u32, u32, &str); 3] = [
        (
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            DWMWCP_DONOTROUND as u32,
            "corners",
        ),
        (DWMWA_BORDER_COLOR as u32, DWMWA_COLOR_NONE, "border"),
        (
            DWMWA_NCRENDERING_POLICY as u32,
            DWMNCRP_DISABLED as u32,
            "non-client edge",
        ),
    ];
    for (attribute, value, what) in frame {
        #[expect(
            unsafe_code,
            reason = "DwmSetWindowAttribute has no safe binding; every value here is a plain u32 attribute"
        )]
        // SAFETY: `hwnd` is live; the call receives a pointer to a `u32` that
        // outlives it, with its exact size, as the attribute's contract requires.
        let ok = unsafe {
            DwmSetWindowAttribute(hwnd, attribute, (&raw const value).cast(), ATTRIBUTE_SIZE)
        } == 0;
        if !ok {
            tracing::warn!(what, "could not remove part of the ring window's DWM frame");
        }
    }
}

/// Only Windows paints a backdrop behind a transparent window's content.
#[cfg(not(target_os = "windows"))]
pub fn disable_backdrop(window: &gpui::Window) {
    let _ = window;
}

/// Other GPUI backends need no additional native window configuration here.
#[cfg(not(target_os = "macos"))]
pub fn configure_windows() {}

/// Owner of the native click-away event monitor; dropping it removes the
/// monitor. Create and drop on the main thread.
#[cfg(target_os = "macos")]
pub struct ClickAwayMonitor(objc2::rc::Retained<objc2::runtime::AnyObject>);

#[cfg(target_os = "macos")]
impl Drop for ClickAwayMonitor {
    #[expect(
        unsafe_code,
        reason = "NSEvent::removeMonitor is plain AppKit FFI; the token is exactly what addGlobalMonitor returned"
    )]
    fn drop(&mut self) {
        // SAFETY: `self.0` is the monitor token returned by
        // `addGlobalMonitorForEventsMatchingMask_handler`, removed only once.
        unsafe { objc2_app_kit::NSEvent::removeMonitor(&self.0) };
    }
}

/// Invoke `on_mouse_down` for every mouse-down that macOS delivers to *other*
/// applications, for as long as the returned monitor is held.
///
/// Global `NSEvent` monitors never see events routed to this process's own
/// windows and cannot consume the events they observe — together exactly the
/// ring's click-away contract: clicks on the ring keep hitting the GPUI
/// handlers they always did, while a click anywhere else can dismiss the ring
/// without being swallowed. Must be called on the main thread (returns `None`
/// off it); the handler runs on the main run loop.
#[cfg(target_os = "macos")]
pub fn watch_clicks_outside(on_mouse_down: impl Fn() + 'static) -> Option<ClickAwayMonitor> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSEvent, NSEventMask};

    MainThreadMarker::new()?;
    let handler: block2::RcBlock<dyn Fn(std::ptr::NonNull<NSEvent>)> =
        block2::RcBlock::new(move |_event| on_mouse_down());
    NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
        NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown,
        &handler,
    )
    .map(ClickAwayMonitor)
}

/// Away from macOS no global click monitor is available; the ring keeps its
/// in-window dismissal paths (center ×, slot activation, timeout).
#[cfg(not(target_os = "macos"))]
pub struct ClickAwayMonitor(());

#[cfg(not(target_os = "macos"))]
pub fn watch_clicks_outside(_on_mouse_down: impl Fn() + 'static) -> Option<ClickAwayMonitor> {
    None
}

/// One display's global geometry, in the same top-left-origin global point
/// space that `openlogi_hook::cursor_position()` reports.
pub struct CursorDisplay {
    /// Native display id; on macOS the `CGDirectDisplayID`, numerically equal
    /// to GPUI's `DisplayId` for the same display.
    pub id: u64,
    /// Global origin (top-left corner) of the display, in points.
    pub origin: (f64, f64),
    /// Display size in points.
    pub size: (f64, f64),
}

/// Find the display whose global bounds contain the point `(x, y)`.
///
/// GPUI's `PlatformDisplay::bounds()` zeroes every display's origin (window
/// bounds are display-relative), so mapping a global cursor position to its
/// display has to go through CoreGraphics.
#[cfg(target_os = "macos")]
#[expect(
    unsafe_code,
    reason = "CGGetActiveDisplayList/CGDisplayBounds are plain C FFI; GPUI exposes no global display bounds"
)]
pub fn display_containing(x: f64, y: f64) -> Option<CursorDisplay> {
    use core_graphics::display::{CGDisplayBounds, CGGetActiveDisplayList};

    const MAX_DISPLAYS: u32 = 32;
    let mut ids = [0u32; MAX_DISPLAYS as usize];
    let mut count = 0u32;
    // SAFETY: the list write is bounded by the capacity we pass; `count`
    // reports how many entries were actually filled.
    let result = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if result != 0 {
        return None;
    }
    ids.iter().take(count as usize).find_map(|&id| {
        // SAFETY: side-effect-free C getter on an id from the active list.
        let bounds = unsafe { CGDisplayBounds(id) };
        let contains = x >= bounds.origin.x
            && x < bounds.origin.x + bounds.size.width
            && y >= bounds.origin.y
            && y < bounds.origin.y + bounds.size.height;
        contains.then(|| CursorDisplay {
            id: u64::from(id),
            origin: (bounds.origin.x, bounds.origin.y),
            size: (bounds.size.width, bounds.size.height),
        })
    })
}

/// Away from macOS the GPUI display list already carries global origins, so
/// there is nothing to resolve natively.
#[cfg(not(target_os = "macos"))]
pub fn display_containing(_x: f64, _y: f64) -> Option<CursorDisplay> {
    None
}
