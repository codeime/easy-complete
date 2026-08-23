//! macOS overlay window policy.
//!
//! `macos.rs` is `cfg(macos)` and talks to AppKit. This module is compiled
//! on every OS so Linux CI pins the things that would otherwise only run on
//! a Mac: `NSScreen.screens[0]` (not `mainScreen`) as the Quartz origin, the
//! Cocoa Y flip, a non-activating floating panel, and the frame-echo schedule
//! that stops an AppKit move notification from livelocking the drain.
//!
//! Live `orderOut` / `setFrameTopLeftPoint` still needs a macOS host.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// `NSWindowStyleMaskNonactivatingPanel` — overlay must not steal key focus.
pub const NS_WINDOW_STYLE_NONACTIVATING_PANEL: u64 = 1 << 7;
/// `NSWindowAnimationBehaviorNone`
pub const NS_WINDOW_ANIMATION_BEHAVIOR_NONE: i64 = 2;
/// `NSFloatingWindowLevel`
pub const NS_FLOATING_WINDOW_LEVEL: i64 = 3;
/// `NSWindowCollectionBehaviorCanJoinAllSpaces`
pub const NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES: u64 = 1 << 0;
/// `NSWindowCollectionBehaviorFullScreenAuxiliary`
pub const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: u64 = 1 << 8;
/// `NSWindowCollectionBehaviorStationary`
pub const NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY: u64 = 1 << 4;

/// `NSScreen.screens[0]` is the menu-bar / global-origin display.
pub const MACOS_PRIMARY_SCREEN_INDEX: usize = 0;

pub const MAX_OVERLAY_FRAME_RETRIES: u8 = 4;
pub const FRAME_EPS: f64 = 0.5;

/// Invalidates queued frame requests when the overlay is hidden and lets only
/// the newest position request bring the singleton overlay window forward.
static OVERLAY_FRAME_EPOCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub struct OverlayFrameRequest {
    pub window: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub epoch: u64,
    pub retries: u8,
}

impl OverlayFrameRequest {
    pub fn same_geometry(&self, other: &Self) -> bool {
        overlay_geometry_close(
            (self.window, self.x, self.y, self.width, self.height),
            (other.window, other.x, other.y, other.width, other.height),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayFrameSchedule {
    /// Same caret frame as the request already in flight or just applied.
    /// Must not bump the epoch: size retries keep the original epoch, and
    /// `overlay_frame_request_is_current` is an equality check.
    IgnoreEcho,
    /// A genuinely new placement. Caller bumps the epoch and enqueues.
    Enqueue,
}

pub struct OverlayFrameQueue {
    pub pending: Option<OverlayFrameRequest>,
    pub applied: Option<OverlayFrameRequest>,
}

pub fn decide_overlay_frame_schedule(
    next: &OverlayFrameRequest,
    pending: Option<&OverlayFrameRequest>,
    applied: Option<&OverlayFrameRequest>,
) -> OverlayFrameSchedule {
    if pending.is_some_and(|current| current.same_geometry(next)) {
        return OverlayFrameSchedule::IgnoreEcho;
    }
    // `drain` publishes `applied` before `apply`. The AppKit write can echo
    // through GPUI after a newer request has already bumped the epoch and
    // taken `pending`. Matching `applied` — not its epoch — is what keeps
    // that echo from overwriting the newer frame. Hide-then-reshow works
    // because `invalidate_overlay_frame_requests` clears `applied`.
    if applied.is_some_and(|current| current.same_geometry(next)) {
        return OverlayFrameSchedule::IgnoreEcho;
    }
    OverlayFrameSchedule::Enqueue
}

pub fn retain_pending_after_apply(
    applied: OverlayFrameRequest,
    pending: Option<OverlayFrameRequest>,
) -> Option<OverlayFrameRequest> {
    match pending {
        Some(next) if next.same_geometry(&applied) => None,
        other => other,
    }
}

pub fn overlay_geometry_close(left: (usize, f64, f64, f64, f64), right: (usize, f64, f64, f64, f64)) -> bool {
    left.0 == right.0
        && (left.1 - right.1).abs() < FRAME_EPS
        && (left.2 - right.2).abs() < FRAME_EPS
        && (left.3 - right.3).abs() < FRAME_EPS
        && (left.4 - right.4).abs() < FRAME_EPS
}

pub fn overlay_frame_queue() -> &'static Mutex<OverlayFrameQueue> {
    static QUEUE: OnceLock<Mutex<OverlayFrameQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        Mutex::new(OverlayFrameQueue {
            pending: None,
            applied: None,
        })
    })
}

pub fn begin_overlay_frame_request() -> u64 {
    OVERLAY_FRAME_EPOCH.fetch_add(1, Ordering::SeqCst) + 1
}

pub fn invalidate_overlay_frame_requests() {
    OVERLAY_FRAME_EPOCH.fetch_add(1, Ordering::SeqCst);
    let mut queue = overlay_frame_queue().lock().unwrap_or_else(|err| err.into_inner());
    queue.pending = None;
    queue.applied = None;
}

pub fn overlay_frame_request_is_current(epoch: u64) -> bool {
    OVERLAY_FRAME_EPOCH.load(Ordering::SeqCst) == epoch
}

/// Cocoa frame origin is bottom-left. Compare against a Quartz top-left Y.
pub fn cocoa_frame_top_left_close(origin_x: f64, origin_y: f64, height: f64, x: f64, top_left_y: f64) -> bool {
    (origin_x - x).abs() < FRAME_EPS && (origin_y + height - top_left_y).abs() < FRAME_EPS
}

pub fn cocoa_frame_size_close(width: f64, height: f64, want_width: f64, want_height: f64) -> bool {
    (width - want_width).abs() < FRAME_EPS && (height - want_height).abs() < FRAME_EPS
}

/// Only the caret edge needs an AppKit write. Size mismatch is GPUI's deferred
/// `setContentSize` and is waited out by [`overlay_frame_should_retry`]. Writing
/// `setFrameTopLeftPoint` for a size-only mismatch was the livelock trigger:
/// the move notification asked us to place the same frame again.
pub fn overlay_frame_needs_reposition(top_left_matches: bool) -> bool {
    !top_left_matches
}

/// GPUI defers `setContentSize` onto the foreground executor. If we pin the
/// top-left before that resize lands, AppKit keeps the bottom-left origin and
/// the caret-relative edge drifts — most visibly when the list shrinks.
pub fn overlay_frame_should_retry(size_matches: bool, retries: u8) -> bool {
    !size_matches && retries < MAX_OVERLAY_FRAME_RETRIES
}

/// Convert a global Quartz (top-left, origin at the top of the primary display)
/// Y into a Cocoa `NSWindow` frame origin Y (bottom-left of the primary display).
pub fn quartz_y_to_cocoa_frame_y(quartz_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - quartz_y - height
}

/// Flip one Cocoa screen frame (bottom-left origin) into the Quartz rect space
/// that Accessibility caret coordinates use (top-left origin, `y` growing down
/// from the primary display's top edge).
pub fn cocoa_screen_to_quartz(frame: (f64, f64, f64, f64), primary_top: f64) -> (f64, f64, f64, f64) {
    let (x, y, width, height) = frame;
    (x, primary_top - (y + height), width, height)
}

pub fn macos_primary_screen_index() -> usize {
    MACOS_PRIMARY_SCREEN_INDEX
}

/// `NSScreen.mainScreen` is whichever display holds the key window. The overlay
/// origin must not follow it: an external monitor would shift the Y flip.
pub fn macos_overlay_anchors_to_main_screen() -> bool {
    false
}

pub fn macos_overlay_style_mask() -> u64 {
    NS_WINDOW_STYLE_NONACTIVATING_PANEL
}

pub fn macos_overlay_animation_behavior() -> i64 {
    NS_WINDOW_ANIMATION_BEHAVIOR_NONE
}

pub fn macos_overlay_window_level() -> i64 {
    NS_FLOATING_WINDOW_LEVEL
}

pub fn macos_overlay_collection_behavior() -> u64 {
    NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES
        | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY
        | NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY
}

pub fn macos_overlay_activates() -> bool {
    macos_overlay_style_mask() & NS_WINDOW_STYLE_NONACTIVATING_PANEL == 0
}

pub fn macos_overlay_park_orders_out() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(cocoa_frame_top_left_close(10.0, 20.0, 140.0, 10.2, 160.2));
        assert!(!cocoa_frame_top_left_close(10.0, 20.0, 140.0, 40.0, 160.0));
    }

    #[test]
    fn frame_size_comparison_tolerates_subpixel_jitter() {
        assert!(cocoa_frame_size_close(320.0, 140.0, 320.2, 139.8));
        assert!(!cocoa_frame_size_close(320.0, 140.0, 320.0, 88.0));
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
        assert!(!macos_overlay_anchors_to_main_screen());
        assert_eq!(macos_primary_screen_index(), 0);
    }

    #[test]
    fn overlay_retries_until_gpui_resize_lands() {
        assert!(overlay_frame_should_retry(false, 0));
        assert!(overlay_frame_should_retry(false, 3));
        assert!(!overlay_frame_should_retry(false, 4));
        assert!(!overlay_frame_should_retry(true, 0));
    }

    #[test]
    fn size_only_mismatch_does_not_write_the_appkit_frame() {
        // GPUI's deferred resize is waited out. Touching AppKit for a size-only
        // mismatch is what fed the main-queue livelock.
        assert!(!overlay_frame_needs_reposition(true));
        assert!(overlay_frame_needs_reposition(false));
    }

    fn sample_request(x: f64, y: f64) -> OverlayFrameRequest {
        OverlayFrameRequest {
            window: 1,
            x,
            y,
            width: 320.0,
            height: 140.0,
            epoch: 7,
            retries: 0,
        }
    }

    #[test]
    fn identical_geometry_is_not_a_newer_frame_request() {
        let applied = sample_request(10.0, 20.0);
        let echo = OverlayFrameRequest {
            epoch: 8,
            retries: 0,
            ..applied
        };
        assert!(applied.same_geometry(&echo));
        assert!(retain_pending_after_apply(applied, Some(echo)).is_none());
        assert!(overlay_geometry_close(
            (1, 10.0, 20.0, 320.0, 140.0),
            (1, 10.2, 20.4, 320.3, 139.8)
        ));
        assert!(!overlay_geometry_close(
            (1, 10.0, 20.0, 320.0, 140.0),
            (1, 40.0, 20.0, 320.0, 140.0)
        ));
    }

    #[test]
    fn a_moved_caret_still_replaces_the_pending_frame() {
        let applied = sample_request(10.0, 20.0);
        let moved = sample_request(10.0, 80.0);
        assert_eq!(
            retain_pending_after_apply(applied, Some(moved)).map(|r| r.y),
            Some(80.0)
        );
    }

    #[test]
    fn echo_of_the_live_applied_frame_does_not_advance_epoch() {
        // After drain takes `pending`, apply's AppKit write echoes the same
        // caret frame. That echo must not look like a newer request: retries
        // keep the applied epoch, and `overlay_frame_request_is_current` is
        // an equality check.
        let applied = sample_request(10.0, 20.0);
        let echo = OverlayFrameRequest {
            epoch: 0,
            retries: 0,
            ..applied
        };
        assert_eq!(
            decide_overlay_frame_schedule(&echo, None, Some(&applied)),
            OverlayFrameSchedule::IgnoreEcho
        );
    }

    #[test]
    fn echo_matching_pending_geometry_is_ignored() {
        let pending = sample_request(10.0, 20.0);
        let echo = OverlayFrameRequest {
            epoch: pending.epoch + 1,
            ..pending
        };
        assert_eq!(
            decide_overlay_frame_schedule(&echo, Some(&pending), None),
            OverlayFrameSchedule::IgnoreEcho
        );
    }

    #[test]
    fn hide_then_reshow_same_geometry_is_a_new_request() {
        let applied = sample_request(10.0, 20.0);
        let again = OverlayFrameRequest { epoch: 0, ..applied };
        // `invalidate_overlay_frame_requests` clears `applied`. Same geometry
        // is only an echo while that slot still holds the live frame.
        assert_eq!(
            decide_overlay_frame_schedule(&again, None, None),
            OverlayFrameSchedule::Enqueue
        );
    }

    #[test]
    fn echo_of_applied_does_not_clobber_a_newer_pending_frame() {
        let applied = sample_request(10.0, 20.0);
        let newer = sample_request(10.0, 80.0);
        let echo = OverlayFrameRequest { epoch: 0, ..applied };
        // B already bumped the epoch. The in-flight apply of A can still echo;
        // treating that as Enqueue would overwrite B, then retain would drop it.
        assert_eq!(
            decide_overlay_frame_schedule(&echo, Some(&newer), Some(&applied)),
            OverlayFrameSchedule::IgnoreEcho
        );
        assert_eq!(
            retain_pending_after_apply(applied, Some(newer)).map(|r| r.y),
            Some(80.0)
        );
    }

    #[test]
    fn a_moved_caret_is_enqueued_even_while_a_retry_is_in_flight() {
        let applied = sample_request(10.0, 20.0);
        let moved = sample_request(10.0, 80.0);
        assert_eq!(
            decide_overlay_frame_schedule(&moved, None, Some(&applied)),
            OverlayFrameSchedule::Enqueue
        );
    }

    #[test]
    fn overlay_is_a_nonactivating_floating_panel_that_joins_all_spaces() {
        assert!(!macos_overlay_activates());
        assert_eq!(macos_overlay_style_mask(), NS_WINDOW_STYLE_NONACTIVATING_PANEL);
        assert_eq!(macos_overlay_window_level(), NS_FLOATING_WINDOW_LEVEL);
        assert_eq!(macos_overlay_animation_behavior(), NS_WINDOW_ANIMATION_BEHAVIOR_NONE);
        let behavior = macos_overlay_collection_behavior();
        assert_eq!(behavior & NS_WINDOW_COLLECTION_BEHAVIOR_CAN_JOIN_ALL_SPACES, 1);
        assert_eq!(behavior & NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY, 1 << 8);
        assert_eq!(behavior & NS_WINDOW_COLLECTION_BEHAVIOR_STATIONARY, 1 << 4);
        assert!(macos_overlay_park_orders_out());
    }

    #[test]
    fn macos_host_uses_screens_zero_not_main_screen() {
        let src = include_str!("macos.rs");
        assert!(
            src.contains("macos_primary_screen_index()"),
            "AppKit host must ask this module for screens[0]"
        );
        assert!(
            src.contains("msg_send![class!(NSScreen), screens]"),
            "primary display is NSScreen.screens, not a synthetic list"
        );
        assert!(
            !src.contains("msg_send![class!(NSScreen), mainScreen]"),
            "mainScreen is the focused display and breaks external-monitor placement"
        );
        assert!(
            src.contains("quartz_y_to_cocoa_frame_y"),
            "AppKit place still flips Quartz Y through the shared formula"
        );
        assert!(
            src.contains("decide_overlay_frame_schedule"),
            "AppKit place still uses the shared echo schedule"
        );
        assert!(
            src.contains("orderOut:") && src.contains("orderFrontRegardless"),
            "park is orderOut, show is orderFrontRegardless"
        );
        assert!(
            src.contains("macos_overlay_style_mask()"),
            "style mask must stay the non-activating panel from this module"
        );
        assert!(
            !src.contains("fn quartz_y_to_cocoa_frame_y"),
            "do not fork the Quartz Y flip back into macos.rs"
        );
        assert!(
            !src.contains("fn decide_overlay_frame_schedule"),
            "do not fork the frame-echo schedule back into macos.rs"
        );
    }
}
