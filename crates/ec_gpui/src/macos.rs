//! macOS overlay hardening: floating level, join all spaces, non-activating.

#![allow(unexpected_cfgs)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSArray, NSPoint, NSRect, NSString};
use objc::rc::autoreleasepool;
use objc::{class, msg_send, sel, sel_impl};

/// `NSWindowStyleMaskNonactivatingPanel` — overlay must not steal key focus.
const NS_WINDOW_STYLE_NONACTIVATING_PANEL: u64 = 1 << 7;
/// `NSWindowAnimationBehaviorNone`
const NS_WINDOW_ANIMATION_BEHAVIOR_NONE: i64 = 2;

const NS_FLOATING_WINDOW_LEVEL: i64 = 3;
const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: u64 = 1 << 0;
const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: u64 = 1 << 8;
const NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY: u64 = 1 << 4;

/// Title used to find the GPUI overlay `NSWindow` without touching the dashboard.
pub const OVERLAY_WINDOW_TITLE: &str = "Fig Autocomplete";

/// Invalidates queued frame requests when the overlay is hidden and lets only
/// the newest position request bring the singleton overlay window forward.
static OVERLAY_FRAME_EPOCH: AtomicU64 = AtomicU64::new(0);
static OVERLAY_FRAME_DRAIN_SCHEDULED: AtomicBool = AtomicBool::new(false);

const MAX_OVERLAY_FRAME_RETRIES: u8 = 4;

#[derive(Clone, Copy)]
struct OverlayFrameRequest {
    window: usize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    epoch: u64,
    retries: u8,
}

fn pending_overlay_frame() -> &'static Mutex<Option<OverlayFrameRequest>> {
    static PENDING: OnceLock<Mutex<Option<OverlayFrameRequest>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(None))
}

fn begin_overlay_frame_request() -> u64 {
    OVERLAY_FRAME_EPOCH.fetch_add(1, Ordering::SeqCst) + 1
}

fn invalidate_overlay_frame_requests() {
    OVERLAY_FRAME_EPOCH.fetch_add(1, Ordering::SeqCst);
    *pending_overlay_frame().lock().unwrap_or_else(|err| err.into_inner()) = None;
}

fn overlay_frame_request_is_current(epoch: u64) -> bool {
    OVERLAY_FRAME_EPOCH.load(Ordering::SeqCst) == epoch
}

fn for_each_window_titled(title: Option<&str>, mut f: impl FnMut(id)) {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = title;
        let _ = &mut f;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        autoreleasepool(|| {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            if app == nil {
                return;
            }
            let windows: id = msg_send![app, windows];
            if windows == nil {
                return;
            }
            let wanted: id = match title {
                Some(title) => {
                    let s: id = NSString::alloc(nil).init_str(title);
                    let _: id = msg_send![s, autorelease];
                    s
                },
                None => nil,
            };
            let count = NSArray::count(windows);
            for i in 0..count {
                let window: id = windows.objectAtIndex(i);
                if wanted != nil {
                    let ns_title: id = msg_send![window, title];
                    if ns_title == nil {
                        continue;
                    }
                    let equal: bool = msg_send![ns_title, isEqualToString: wanted];
                    if !equal {
                        continue;
                    }
                }
                f(window);
            }
        });
    }
}

fn set_layer_square(view: id) {
    if view == nil {
        return;
    }
    unsafe {
        let _: () = msg_send![view, setWantsLayer: YES];
        let layer: id = msg_send![view, layer];
        if layer != nil {
            let _: () = msg_send![layer, setCornerRadius: 0.0f64];
            let _: () = msg_send![layer, setMasksToBounds: NO];
        }
    }
}

/// Borderless panel, floating level, join-all-spaces. Call once after the window is created.
/// Calling `setStyleMask` after `setFrame` reintroduces AppKit's titled-panel chrome.
fn configure_overlay_window(window: id) {
    unsafe {
        let _: () = msg_send![window, setLevel: NS_FLOATING_WINDOW_LEVEL];
        let behavior: u64 = NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
            | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY
            | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY;
        let _: () = msg_send![window, setCollectionBehavior: behavior];
        let _: () = msg_send![window, setHidesOnDeactivate: NO];
        let _: () = msg_send![window, setIgnoresMouseEvents: NO];
        let _: () = msg_send![window, setStyleMask: NS_WINDOW_STYLE_NONACTIVATING_PANEL];
        let _: () = msg_send![window, setAnimationBehavior: NS_WINDOW_ANIMATION_BEHAVIOR_NONE];
    }
    polish_overlay_window(window);
}

/// `setStyleMask:` may synchronously emit resize/move notifications. GPUI
/// installs callbacks for those notifications which re-enter its `AppCell`, so
/// applying the native window policy from inside `Entity::update` can fail with
/// `already borrowed` and leave the renderer's viewport stale. Queue it onto
/// the next main-dispatch turn, after the current GPUI update has returned.
#[cfg(target_os = "macos")]
fn schedule_configure_overlay_window(window: id) {
    let window = window as usize;
    dispatch::Queue::main().exec_async(move || {
        autoreleasepool(|| configure_overlay_window(window as id));
    });
}

#[cfg(not(target_os = "macos"))]
fn schedule_configure_overlay_window(window: id) {
    let _ = window;
}

/// No AppKit shadow / rounded theme-frame. Safe to call on every show; does not change style mask.
fn polish_overlay_window(window: id) {
    unsafe {
        let _: () = msg_send![window, setHasShadow: NO];
        let _: () = msg_send![window, setOpaque: NO];
        let clear: id = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![window, setBackgroundColor: clear];
        let content: id = msg_send![window, contentView];
        set_layer_square(content);
        if content != nil {
            let superview: id = msg_send![content, superview];
            set_layer_square(superview);
        }
    }
}

/// Apply overlay window level / space behavior to every NSWindow (spike) or a titled window.
#[cfg(target_os = "macos")]
pub fn harden_overlay_window() {
    harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
}

#[cfg(not(target_os = "macos"))]
pub fn harden_overlay_window() {}

pub fn harden_overlay_window_titled(title: &str) {
    let filter = if title.is_empty() { None } else { Some(title) };
    for_each_window_titled(filter, |window| {
        configure_overlay_window(window);
    });
}

pub fn polish_overlay_window_titled(title: &str) {
    let filter = if title.is_empty() { None } else { Some(title) };
    for_each_window_titled(filter, |window| {
        polish_overlay_window(window);
    });
}

pub fn set_overlay_window_level(level: i64) {
    set_overlay_window_level_for_title(OVERLAY_WINDOW_TITLE, level);
}

pub fn set_overlay_window_level_for_title(title: &str, level: i64) {
    for_each_window_titled(Some(title), |window| unsafe {
        let _: () = msg_send![window, setLevel: level];
    });
}

pub fn set_overlay_visible_titled(title: &str, visible: bool) {
    if visible {
        for_each_window_titled(Some(title), order_overlay_front);
    } else {
        park_overlay_window_titled(title);
    }
}

fn order_overlay_front(window: id) {
    unsafe {
        let _: () = msg_send![window, orderFrontRegardless];
    }
}

fn order_overlay_out(window: id) {
    unsafe {
        let _: () = msg_send![window, orderOut: cocoa::base::nil];
    }
}

/// Defer the AppKit position/show operation until GPUI has released `App`.
/// Content sizing is deliberately handled by `gpui::Window::resize`; changing it
/// through AppKit here makes GPUI's synchronous resize callback re-borrow `App`,
/// which can leave the renderer at a stale viewport and eventually starve events.
fn schedule_overlay_frame(window: id, x: f64, y: f64, width: f64, height: f64, epoch: u64, retries: u8) {
    #[cfg(target_os = "macos")]
    {
        *pending_overlay_frame().lock().unwrap_or_else(|err| err.into_inner()) = Some(OverlayFrameRequest {
            window: window as usize,
            x,
            y,
            width,
            height,
            epoch,
            retries,
        });
        if !OVERLAY_FRAME_DRAIN_SCHEDULED.swap(true, Ordering::AcqRel) {
            queue_overlay_frame_drain();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, x, y, width, height, epoch, retries);
    }
}

#[cfg(target_os = "macos")]
fn queue_overlay_frame_drain() {
    dispatch::Queue::main().exec_async(drain_overlay_frame);
}

#[cfg(target_os = "macos")]
fn drain_overlay_frame() {
    let request = pending_overlay_frame()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take();
    if let Some(request) = request {
        if overlay_frame_request_is_current(request.epoch) {
            apply_overlay_frame(
                request.window as id,
                request.x,
                request.y,
                request.width,
                request.height,
                request.epoch,
                request.retries,
            );
        }
    }

    OVERLAY_FRAME_DRAIN_SCHEDULED.store(false, Ordering::Release);
    let has_newer = pending_overlay_frame()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .is_some();
    if has_newer
        && OVERLAY_FRAME_DRAIN_SCHEDULED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        queue_overlay_frame_drain();
    }
}

/// One display frame at 60Hz. Long enough for GPUI's foreground executor to
/// apply `setContentSize`, short enough that a corrected frame is not visible.
#[cfg(target_os = "macos")]
const OVERLAY_FRAME_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(16);

#[cfg(target_os = "macos")]
fn retry_overlay_frame_after(window: id, x: f64, y: f64, width: f64, height: f64, epoch: u64, retries: u8) {
    let window = window as usize;
    dispatch::Queue::main().exec_after(OVERLAY_FRAME_RETRY_DELAY, move || {
        if overlay_frame_request_is_current(epoch) {
            apply_overlay_frame(window as id, x, y, width, height, epoch, retries);
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn retry_overlay_frame_after(window: id, x: f64, y: f64, width: f64, height: f64, epoch: u64, retries: u8) {
    let _ = (window, x, y, width, height, epoch, retries);
}

fn frame_top_left_close(frame: NSRect, x: f64, top_left_y: f64) -> bool {
    const EPS: f64 = 0.5;
    (frame.origin.x - x).abs() < EPS && (frame.origin.y + frame.size.height - top_left_y).abs() < EPS
}

fn frame_size_close(frame: NSRect, width: f64, height: f64) -> bool {
    const EPS: f64 = 0.5;
    (frame.size.width - width).abs() < EPS && (frame.size.height - height).abs() < EPS
}

/// GPUI defers `setContentSize` onto the foreground executor. If we pin the
/// top-left before that resize lands, AppKit keeps the bottom-left origin and
/// the caret-relative edge drifts — most visibly when the list shrinks.
fn overlay_frame_should_retry(size_matches: bool, retries: u8) -> bool {
    !size_matches && retries < MAX_OVERLAY_FRAME_RETRIES
}

/// Apply the Quartz top-left position and show the already GPUI-sized window.
fn apply_overlay_frame(window: id, x: f64, y: f64, width: f64, height: f64, epoch: u64, retries: u8) {
    let top_left_y = match primary_screen_cocoa() {
        Some((origin_y, primary_height)) => quartz_y_to_cocoa_frame_y(y, 0.0, origin_y, primary_height),
        None => y,
    };
    unsafe {
        let current: NSRect = msg_send![window, frame];
        let visible: bool = msg_send![window, isVisible];
        let size_matches = frame_size_close(current, width, height);
        // Re-pin whenever the caret edge moved, or when GPUI's deferred
        // resize has not landed yet. AppKit keeps the bottom-left origin, so
        // a previously-correct top-left is stale after the list shrinks.
        if !frame_top_left_close(current, x, top_left_y) || !size_matches {
            let _: () = msg_send![window, setFrameTopLeftPoint: NSPoint::new(x, top_left_y)];
        }
        if !visible {
            order_overlay_front(window);
        }
        if overlay_frame_should_retry(size_matches, retries) {
            // Keep the original epoch so a newer user request can still cancel us.
            // Re-dispatching immediately would burn every retry inside the same
            // runloop turn, before GPUI's deferred resize can land.
            retry_overlay_frame_after(window, x, y, width, height, epoch, retries + 1);
        }
    }
}

/// `NSWindow` backing a GPUI window. Title lookup misses hidden / never-shown panels.
pub fn ns_window_from_gpui(window: &gpui::Window) -> Option<id> {
    #[cfg(target_os = "macos")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::AppKit(appkit) => {
                let view = appkit.ns_view.as_ptr() as id;
                if view == nil {
                    return None;
                }
                unsafe {
                    let ns_window: id = msg_send![view, window];
                    if ns_window == nil { None } else { Some(ns_window) }
                }
            },
            _ => None,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        None
    }
}

pub fn harden_overlay_window_handle(window: &gpui::Window) {
    if let Some(ns_window) = ns_window_from_gpui(window) {
        schedule_configure_overlay_window(ns_window);
    } else {
        #[cfg(target_os = "macos")]
        dispatch::Queue::main().exec_async(|| harden_overlay_window_titled(OVERLAY_WINDOW_TITLE));
        #[cfg(not(target_os = "macos"))]
        harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
    }
}

pub fn park_overlay_window_handle(window: &gpui::Window) {
    invalidate_overlay_frame_requests();
    if let Some(ns_window) = ns_window_from_gpui(window) {
        order_overlay_out(ns_window);
    } else {
        order_overlay_out_titled(OVERLAY_WINDOW_TITLE);
    }
}

pub fn set_overlay_visible_handle(window: &gpui::Window, visible: bool) {
    if let Some(ns_window) = ns_window_from_gpui(window) {
        if visible {
            order_overlay_front(ns_window);
        } else {
            invalidate_overlay_frame_requests();
            order_overlay_out(ns_window);
        }
    } else {
        set_overlay_visible_titled(OVERLAY_WINDOW_TITLE, visible);
    }
}

pub fn set_overlay_frame_handle(window: &gpui::Window, x: f64, y: f64, width: f64, height: f64) {
    let epoch = begin_overlay_frame_request();
    if let Some(ns_window) = ns_window_from_gpui(window) {
        schedule_overlay_frame(ns_window, x, y, width, height, epoch, 0);
    } else {
        let title = OVERLAY_WINDOW_TITLE.to_string();
        #[cfg(target_os = "macos")]
        dispatch::Queue::main().exec_async(move || {
            if overlay_frame_request_is_current(epoch) {
                apply_overlay_frame_titled(&title, x, y, width, height, epoch);
            }
        });
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (title, x, y, width, height, epoch);
        }
    }
}

/// Hide the overlay the way the WebView host did: `orderOut`, keep the last size.
pub fn park_overlay_window_titled(title: &str) {
    invalidate_overlay_frame_requests();
    order_overlay_out_titled(title);
}

fn order_overlay_out_titled(title: &str) {
    for_each_window_titled(Some(title), order_overlay_out);
}

/// Convert a global Quartz (top-left, origin at the top of the primary display)
/// Y into a Cocoa `NSWindow` frame origin Y (bottom-left of the primary display).
pub fn quartz_y_to_cocoa_frame_y(quartz_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - quartz_y - height
}

/// The display that anchors the global coordinate space: `NSScreen.screens[0]`,
/// the one carrying the menu bar, whose Cocoa origin is `(0, 0)` and whose top
/// edge is Quartz `y = 0`.
///
/// Deliberately **not** `NSScreen.mainScreen`, which is whichever display holds
/// the key window and therefore moves with focus. On a single display the two
/// agree; with an external monitor `mainScreen` shifts the flip anchor by the
/// difference between the two screens' top edges and the overlay lands at the
/// wrong height.
#[cfg(target_os = "macos")]
fn primary_screen() -> Option<id> {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens == nil || NSArray::count(screens) == 0 {
            return None;
        }
        let primary: id = screens.objectAtIndex(0);
        if primary == nil { None } else { Some(primary) }
    }
}

fn primary_screen_cocoa() -> Option<(f64, f64)> {
    #[cfg(target_os = "macos")]
    unsafe {
        let screen = primary_screen()?;
        let frame: NSRect = msg_send![screen, frame];
        Some((frame.origin.y, frame.size.height))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Flip one Cocoa screen frame (bottom-left origin) into the Quartz rect space
/// that Accessibility caret coordinates use (top-left origin, `y` growing down
/// from the primary display's top edge).
fn cocoa_screen_to_quartz(frame: (f64, f64, f64, f64), primary_top: f64) -> (f64, f64, f64, f64) {
    let (x, y, width, height) = frame;
    (x, primary_top - (y + height), width, height)
}

/// Screens as Quartz rects `(x, y, width, height)` with origin at the top-left of
/// the primary display — the same space Accessibility caret coordinates use.
pub fn screens_quartz() -> Vec<(f64, f64, f64, f64)> {
    #[cfg(target_os = "macos")]
    unsafe {
        autoreleasepool(|| {
            let screens: id = msg_send![class!(NSScreen), screens];
            if screens == nil {
                return Vec::new();
            }
            let Some(primary) = primary_screen() else {
                return Vec::new();
            };
            let primary_frame: NSRect = msg_send![primary, frame];
            let primary_top = primary_frame.origin.y + primary_frame.size.height;
            let count = NSArray::count(screens);
            let mut out = Vec::with_capacity(count as usize);
            let mut push_screen = |screen: id| {
                let frame: NSRect = msg_send![screen, frame];
                out.push(cocoa_screen_to_quartz(
                    (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height),
                    primary_top,
                ));
            };

            // The primary display stays first. IME caret coordinates arrive in
            // Cocoa's global bottom-left space, whose Y conversion is anchored
            // to this screen rather than whichever display holds the caret, and
            // callers read `screens[0]` for that anchor.
            push_screen(primary);
            for i in 0..count {
                let screen: id = screens.objectAtIndex(i);
                if screen != primary {
                    push_screen(screen);
                }
            }
            out
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// Place the overlay in global Quartz (top-left) coordinates by converting to AppKit's
/// top-left point, matching tao `set_outer_position`.
pub fn set_overlay_frame_titled(title: &str, x: f64, y: f64, width: f64, height: f64) {
    let epoch = begin_overlay_frame_request();
    let title = title.to_string();
    #[cfg(target_os = "macos")]
    dispatch::Queue::main().exec_async(move || {
        if overlay_frame_request_is_current(epoch) {
            apply_overlay_frame_titled(&title, x, y, width, height, epoch);
        }
    });
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, x, y, width, height, epoch);
    }
}

fn apply_overlay_frame_titled(title: &str, x: f64, y: f64, width: f64, height: f64, epoch: u64) {
    for_each_window_titled(Some(title), |window| {
        apply_overlay_frame(window, x, y, width, height, epoch, 0);
    });
}

/// Best-effort read of the process-wide AppKit appearance. Defaults to dark when
/// NSApplication is not ready yet (common during early startup).
pub fn system_appearance_is_dark() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        autoreleasepool(|| {
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            if app == nil {
                return true;
            }
            let appearance: id = msg_send![app, effectiveAppearance];
            if appearance == nil {
                return true;
            }
            let name: id = msg_send![appearance, name];
            if name == nil {
                return true;
            }
            let dark: id = NSString::alloc(nil).init_str("NSAppearanceNameDarkAqua");
            let _: id = msg_send![dark, autorelease];
            let vibrant: id = NSString::alloc(nil).init_str("NSAppearanceNameVibrantDark");
            let _: id = msg_send![vibrant, autorelease];
            let is_dark: bool = msg_send![name, isEqualToString: dark];
            let is_vibrant: bool = msg_send![name, isEqualToString: vibrant];
            is_dark || is_vibrant
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use cocoa::foundation::{NSPoint, NSRect, NSSize};

    use super::{
        begin_overlay_frame_request, cocoa_screen_to_quartz, frame_size_close, frame_top_left_close,
        invalidate_overlay_frame_requests, overlay_frame_request_is_current, overlay_frame_should_retry,
        quartz_y_to_cocoa_frame_y,
    };

    #[test]
    fn only_latest_unhidden_frame_request_can_show() {
        let old = begin_overlay_frame_request();
        let latest = begin_overlay_frame_request();
        assert!(!overlay_frame_request_is_current(old));
        assert!(overlay_frame_request_is_current(latest));
        invalidate_overlay_frame_requests();
        assert!(!overlay_frame_request_is_current(latest));
    }

    #[test]
    fn quartz_to_cocoa_uses_primary_display_height() {
        // 900px primary, caret 100px from the top, 140px overlay → Cocoa origin 660.
        assert_eq!(quartz_y_to_cocoa_frame_y(100.0, 140.0, 0.0, 900.0), 660.0);
    }

    #[test]
    fn quartz_to_cocoa_top_left_matches_tao_set_frame_top_left() {
        // Quartz y=100 on a 900px primary → Cocoa top-left y=800.
        assert_eq!(quartz_y_to_cocoa_frame_y(100.0, 0.0, 0.0, 900.0), 800.0);
    }

    #[test]
    fn quartz_to_cocoa_accounts_for_primary_origin() {
        assert_eq!(quartz_y_to_cocoa_frame_y(50.0, 20.0, 200.0, 800.0), 930.0);
    }

    #[test]
    fn frame_top_left_comparison_tolerates_subpixel_jitter() {
        let a = NSRect::new(NSPoint::new(10.0, 20.0), NSSize::new(320.0, 140.0));
        assert!(frame_top_left_close(a, 10.2, 160.2));
        assert!(!frame_top_left_close(a, 40.0, 160.0));
    }

    #[test]
    fn frame_size_comparison_tolerates_subpixel_jitter() {
        let a = NSRect::new(NSPoint::new(10.0, 20.0), NSSize::new(320.0, 140.0));
        assert!(frame_size_close(a, 320.2, 139.8));
        assert!(!frame_size_close(a, 320.0, 88.0));
    }

    #[test]
    fn primary_screen_maps_onto_quartz_origin() {
        let primary = (0.0, 0.0, 1440.0, 900.0);
        assert_eq!(cocoa_screen_to_quartz(primary, 900.0), (0.0, 0.0, 1440.0, 900.0));
    }

    #[test]
    fn taller_external_monitor_reaches_above_quartz_zero() {
        // 1440x900 laptop as primary, a 2560x1440 display to its right and
        // bottom-aligned with it: Cocoa (1440, 0). Its top edge sits 540px above
        // the primary's, which is negative Y in the caret's Quartz space.
        let external = (1440.0, 0.0, 2560.0, 1440.0);
        assert_eq!(
            cocoa_screen_to_quartz(external, 900.0),
            (1440.0, -540.0, 2560.0, 1440.0)
        );
    }

    #[test]
    fn external_monitor_below_the_primary_gets_positive_quartz_y() {
        // Display stacked under the laptop: its top edge starts where the
        // primary's bottom edge ends.
        let below = (0.0, -1080.0, 1920.0, 1080.0);
        assert_eq!(cocoa_screen_to_quartz(below, 900.0), (0.0, 900.0, 1920.0, 1080.0));
    }

    #[test]
    fn quartz_flip_is_anchored_to_the_primary_not_the_focused_screen() {
        // Caret 200px down the global Quartz space with a 140px overlay. The
        // answer must come from the 900px primary; anchoring to the bottom-
        // aligned 1440px external display that holds the key window would drop
        // the overlay 540px too low.
        assert_eq!(quartz_y_to_cocoa_frame_y(200.0, 140.0, 0.0, 900.0), 560.0);
        assert_eq!(quartz_y_to_cocoa_frame_y(200.0, 140.0, 0.0, 1440.0), 1100.0);
    }

    #[test]
    fn overlay_retries_until_gpui_resize_lands() {
        assert!(overlay_frame_should_retry(false, 0));
        assert!(overlay_frame_should_retry(false, 3));
        assert!(!overlay_frame_should_retry(false, 4));
        assert!(!overlay_frame_should_retry(true, 0));
    }
}
