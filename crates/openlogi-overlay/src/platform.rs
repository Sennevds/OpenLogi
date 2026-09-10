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

/// Clip `window` to a circle, so nothing outside the dial can paint.
///
/// The ring is round but its window is a rectangle, and GPUI's Windows backend
/// paints that whole rectangle — as a tint, an acrylic backdrop, or just an
/// opaque clear colour, depending on the compositor's mood and the user's
/// transparency setting. Chasing each of those is a losing game; a window
/// region settles it structurally. Pixels outside the ellipse are not part of
/// the window, so there is nowhere for any of them to be drawn.
///
/// Takes the window rather than enumerating the thread's windows: an
/// agent-invoked ring opens a fresh window per invocation while the previous
/// one is still closing, and "every window on this thread" is then ambiguous
/// about which one is the live ring.
#[cfg(target_os = "windows")]
pub fn clip_to_circle(window: &gpui::Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{HWND, RECT, TRUE};
    use windows_sys::Win32::Graphics::Gdi::{CreateEllipticRgn, DeleteObject, SetWindowRgn};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;

    // The trait method by full path: GPUI's inherent `Window::window_handle`
    // returns its own `AnyWindowHandle` and would shadow this one.
    let handle = match HasWindowHandle::window_handle(window) {
        Ok(handle) => handle.as_raw(),
        Err(error) => {
            tracing::warn!(%error, "ring window exposes no native handle — staying rectangular");
            return;
        }
    };
    let RawWindowHandle::Win32(win32) = handle else {
        tracing::warn!("ring window is not a Win32 window — staying rectangular");
        return;
    };
    let hwnd: HWND = win32.hwnd.get() as HWND;

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    #[expect(
        unsafe_code,
        reason = "GetWindowRect/CreateEllipticRgn/SetWindowRgn are the only way to give a window a non-rectangular shape"
    )]
    // SAFETY: `hwnd` is the handle GPUI just reported for a live window, and
    // `rect` is a live, writable `RECT` for the duration of the call.
    let measured = unsafe { GetWindowRect(hwnd, &raw mut rect) } != 0;
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if !measured || width <= 0 || height <= 0 {
        tracing::warn!(
            width,
            height,
            "could not measure the ring window — staying rectangular"
        );
        return;
    }

    #[expect(
        unsafe_code,
        reason = "see above — GDI region APIs are the shape mechanism"
    )]
    // SAFETY: a pure GDI constructor over by-value bounds; null on failure,
    // which is checked before the region is handed to the window.
    let region = unsafe { CreateEllipticRgn(0, 0, width, height) };
    if region.is_null() {
        tracing::warn!("could not create the ring's elliptic region");
        return;
    }
    #[expect(
        unsafe_code,
        reason = "see above — GDI region APIs are the shape mechanism"
    )]
    // SAFETY: `hwnd` is valid and `region` was just created. On success the
    // window takes ownership and frees it; on failure it does not, so the
    // region is still owned here and is deleted below.
    let applied = unsafe { SetWindowRgn(hwnd, region, TRUE) } != 0;
    if applied {
        tracing::debug!(width, height, "ring window clipped to a circle");
    } else {
        #[expect(
            unsafe_code,
            reason = "see above — GDI region APIs are the shape mechanism"
        )]
        // SAFETY: `SetWindowRgn` failed, so the window never took the region
        // and this still owns it.
        unsafe {
            DeleteObject(region);
        }
        tracing::warn!("SetWindowRgn failed — the ring window stays rectangular");
    }
}

/// Only Windows paints outside a transparent window's content.
#[cfg(not(target_os = "windows"))]
pub fn clip_to_circle(window: &gpui::Window) {
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
