//! X11 overlay window policy.
//!
//! `linux.rs` is `cfg(linux)` and talks to the X server. This module is
//! compiled on every OS so Linux CI pins D3 without a display: a GPUI
//! `PopUp` is `_NET_WM_WINDOW_TYPE_NOTIFICATION`, stays `_NET_WM_STATE_ABOVE`,
//! never sends `_NET_ACTIVE_WINDOW`, parks by unmap, and never places from a
//! window rectangle.

#![allow(dead_code)]

/// EWMH type GPUI 0.2.2 already sets for `WindowKind::PopUp`.
pub const NET_WM_WINDOW_TYPE: &str = "_NET_WM_WINDOW_TYPE";
pub const NET_WM_WINDOW_TYPE_NOTIFICATION: &str = "_NET_WM_WINDOW_TYPE_NOTIFICATION";
pub const NET_WM_STATE: &str = "_NET_WM_STATE";
pub const NET_WM_STATE_ABOVE: &str = "_NET_WM_STATE_ABOVE";
pub const NET_WM_STATE_SKIP_TASKBAR: &str = "_NET_WM_STATE_SKIP_TASKBAR";
pub const NET_WM_STATE_SKIP_PAGER: &str = "_NET_WM_STATE_SKIP_PAGER";
pub const NET_ACTIVE_WINDOW: &str = "_NET_ACTIVE_WINDOW";

/// `_NET_WM_STATE` ClientMessage action: add.
pub const NET_WM_STATE_ADD: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayX11Hints {
    pub window_type: &'static str,
    pub state: &'static [&'static str],
    pub sends_active_window: bool,
    pub park_unmaps: bool,
    pub place_sets_size: bool,
}

/// Policy applied after GPUI creates the PopUp and whenever we map/place it.
pub fn overlay_x11_hints() -> OverlayX11Hints {
    OverlayX11Hints {
        window_type: NET_WM_WINDOW_TYPE_NOTIFICATION,
        state: &[NET_WM_STATE_ABOVE, NET_WM_STATE_SKIP_TASKBAR, NET_WM_STATE_SKIP_PAGER],
        sends_active_window: false,
        park_unmaps: true,
        place_sets_size: false,
    }
}

pub fn overlay_x11_activates() -> bool {
    overlay_x11_hints().sends_active_window
}

/// Size stays with GPUI `window.resize`, matching Windows `SWP_NOSIZE`.
pub fn overlay_x11_place_changes_size() -> bool {
    overlay_x11_hints().place_sets_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_is_a_notification_that_stays_above_without_activating() {
        let hints = overlay_x11_hints();
        assert_eq!(hints.window_type, NET_WM_WINDOW_TYPE_NOTIFICATION);
        assert!(hints.state.contains(&NET_WM_STATE_ABOVE));
        assert!(hints.state.contains(&NET_WM_STATE_SKIP_TASKBAR));
        assert!(hints.state.contains(&NET_WM_STATE_SKIP_PAGER));
        assert!(!hints.sends_active_window);
        assert!(!overlay_x11_activates());
        assert_ne!(hints.window_type, NET_ACTIVE_WINDOW);
        assert!(!hints.state.iter().any(|atom| *atom == NET_ACTIVE_WINDOW));
    }

    #[test]
    fn park_unmaps_and_place_leaves_size_to_gpui() {
        let hints = overlay_x11_hints();
        assert!(hints.park_unmaps);
        assert!(!hints.place_sets_size);
        assert!(!overlay_x11_place_changes_size());
        assert_eq!(NET_WM_STATE_ADD, 1);
    }
}
