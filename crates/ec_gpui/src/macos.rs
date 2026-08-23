//! macOS overlay hardening: floating level, join all spaces, non-activating.
//!
//! Flag / Y-flip / frame-echo policy lives in `macos_overlay` so Linux CI can
//! pin it. This module is `cfg(macos)` and performs the AppKit calls.

#![allow(unexpected_cfgs)]

use std::sync::atomic::{AtomicBool, Ordering};

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSArray, NSPoint, NSRect, NSString};
use objc::rc::autoreleasepool;
use objc::{class, msg_send, sel, sel_impl};

use crate::macos_overlay::{
    OverlayFrameRequest, OverlayFrameSchedule, begin_overlay_frame_request, cocoa_frame_size_close,
    cocoa_frame_top_left_close, cocoa_screen_to_quartz, decide_overlay_frame_schedule,
    invalidate_overlay_frame_requests, macos_overlay_animation_behavior, macos_overlay_collection_behavior,
    macos_overlay_style_mask, macos_overlay_window_level, macos_primary_screen_index, overlay_frame_needs_reposition,
    overlay_frame_queue, overlay_frame_request_is_current, overlay_frame_should_retry, quartz_y_to_cocoa_frame_y,
    retain_pending_after_apply,
};
use crate::overlay::OVERLAY_WINDOW_TITLE;

static OVERLAY_FRAME_DRAIN_SCHEDULED: AtomicBool = AtomicBool::new(false);

/// One display frame at 60Hz. Long enough for GPUI's foreground executor to
/// apply `setContentSize`, short enough that a corrected frame is not visible.
#[cfg(target_os = "macos")]
const OVERLAY_FRAME_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(16);

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
        let _: () = msg_send![window, setLevel: macos_overlay_window_level()];
        let _: () = msg_send![window, setCollectionBehavior: macos_overlay_collection_behavior()];
        let _: () = msg_send![window, setHidesOnDeactivate: NO];
        let _: () = msg_send![window, setIgnoresMouseEvents: NO];
        let _: () = msg_send![window, setStyleMask: macos_overlay_style_mask()];
        let _: () = msg_send![window, setAnimationBehavior: macos_overlay_animation_behavior()];
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
fn schedule_overlay_frame(window: id, x: f64, y: f64, width: f64, height: f64) {
    #[cfg(target_os = "macos")]
    {
        let next = OverlayFrameRequest {
            window: window as usize,
            x,
            y,
            width,
            height,
            epoch: 0,
            retries: 0,
        };
        let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
        // `setFrameTopLeftPoint` emits move notifications. GPUI's handler can
        // call back into `set_overlay_frame_handle` with the same caret frame.
        // Bumping the epoch for that echo cancels `retry_overlay_frame_after`,
        // which is the only thing that re-pins the caret edge after GPUI's
        // deferred resize. Replacing it as a "newer" request also reset
        // retries and immediately re-queued the drain — a main-queue livelock.
        if decide_overlay_frame_schedule(&next, queue.pending.as_ref(), queue.applied.as_ref())
            == OverlayFrameSchedule::IgnoreEcho
        {
            return;
        }
        let epoch = begin_overlay_frame_request();
        queue.pending = Some(OverlayFrameRequest { epoch, ..next });
        drop(queue);
        if !OVERLAY_FRAME_DRAIN_SCHEDULED.swap(true, Ordering::AcqRel) {
            queue_overlay_frame_drain();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, x, y, width, height);
    }
}

#[cfg(target_os = "macos")]
fn queue_overlay_frame_drain() {
    dispatch::Queue::main().exec_async(drain_overlay_frame);
}

#[cfg(target_os = "macos")]
fn queue_overlay_frame_drain_later() {
    dispatch::Queue::main().exec_after(OVERLAY_FRAME_RETRY_DELAY, drain_overlay_frame);
}

#[cfg(target_os = "macos")]
fn drain_overlay_frame() {
    // Publish `applied` in the same lock that clears `pending`. Otherwise an
    // echo that arrives after `take` and before `apply` looks like a new
    // request, bumps the epoch, and cancels the size-retry chain.
    let request = {
        let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
        match queue.pending.take() {
            Some(request) if overlay_frame_request_is_current(request.epoch) => {
                queue.applied = Some(request);
                Some(request)
            },
            _ => None,
        }
    };
    if let Some(request) = request {
        apply_overlay_frame(
            request.window as id,
            request.x,
            request.y,
            request.width,
            request.height,
            request.epoch,
            request.retries,
        );
        // A move notification during apply can write the same geometry
        // back into `pending`. Drop it so we do not treat it as a new
        // user request and spin the main queue.
        let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
        queue.pending = retain_pending_after_apply(request, queue.pending.take());
    }

    OVERLAY_FRAME_DRAIN_SCHEDULED.store(false, Ordering::Release);
    let has_newer = overlay_frame_queue()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .pending
        .is_some();
    if has_newer
        && OVERLAY_FRAME_DRAIN_SCHEDULED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        // Yield a frame. Immediate re-dispatch from inside the drain is what
        // livelocked NSApplication when apply kept seeing a "newer" request.
        queue_overlay_frame_drain_later();
    }
}

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
    cocoa_frame_top_left_close(frame.origin.x, frame.origin.y, frame.size.height, x, top_left_y)
}

fn frame_size_close(frame: NSRect, width: f64, height: f64) -> bool {
    cocoa_frame_size_close(frame.size.width, frame.size.height, width, height)
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
        // Re-pin only when the caret edge actually moved. After GPUI's
        // deferred resize lands, the next retry sees a drifted top-left and
        // pins then — AppKit keeps the bottom-left origin, so the caret
        // edge is stale once the list shrinks.
        if overlay_frame_needs_reposition(frame_top_left_close(current, x, top_left_y)) {
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

pub fn invalidate_cached_overlay_x_window() {}

pub fn overlay_placement_scale() -> f64 {
    1.0
}

pub fn set_overlay_frame_handle(window: &gpui::Window, x: f64, y: f64, width: f64, height: f64) -> bool {
    if let Some(ns_window) = ns_window_from_gpui(window) {
        // Epoch is assigned only if this geometry is actually enqueued.
        // Echoes of the live frame must not bump it or in-flight size
        // retries (`retry_overlay_frame_after`) fail the equality check.
        schedule_overlay_frame(ns_window, x, y, width, height);
    } else {
        let epoch = begin_overlay_frame_request();
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
    true
}

/// Hide the overlay the way the WebView host did: `orderOut`, keep the last size.
pub fn park_overlay_window_titled(title: &str) {
    invalidate_overlay_frame_requests();
    order_overlay_out_titled(title);
}

fn order_overlay_out_titled(title: &str) {
    for_each_window_titled(Some(title), order_overlay_out);
}

/// The display that anchors the global coordinate space: `NSScreen.screens[0]`,
/// the one carrying the menu bar, whose Cocoa origin is `(0, 0)` and whose top
/// edge is Quartz `y = 0`.
///
/// Deliberately **not** `NSScreen.mainScreen`, which is whichever display holds
/// the key window and therefore moves with focus. On a single display the two
/// agree; with an external monitor `mainScreen` shifts the flip anchor by the
/// difference between the two screens' top edges and the overlay lands at the
/// wrong height. Index is [`macos_primary_screen_index`].
#[cfg(target_os = "macos")]
fn primary_screen() -> Option<id> {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens == nil || NSArray::count(screens) == 0 {
            return None;
        }
        let primary: id = screens.objectAtIndex(macos_primary_screen_index() as u64);
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

/// Screens as Quartz rects `(x, y, width, height)` with origin at the top-left of
/// the primary display — the same space Accessibility caret coordinates use.
pub fn overlay_screens() -> Vec<(f64, f64, f64, f64)> {
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
