//! HWND / `SetWindowPos` overlay policy.
//!
//! `windows.rs` is `cfg(windows)` and issues the real call. This module is
//! compiled on every OS so Linux CI can pin F5 without a Windows host: never
//! activate, never fight GPUI on size, park with hide+nomove, place in
//! top-left screen space, no titled/window-rect fallback.
//!
//! Live `SetWindowPos` against a GPUI HWND still needs windows-latest and a
//! desktop session.

#![allow(dead_code)]

/// Win32 `SWP_NOSIZE` — size stays with `window.resize`.
pub const WIN32_SWP_NOSIZE: u32 = 0x0001;
/// Win32 `SWP_NOMOVE`.
pub const WIN32_SWP_NOMOVE: u32 = 0x0002;
/// Win32 `SWP_NOACTIVATE` — overlay must not steal console focus.
pub const WIN32_SWP_NOACTIVATE: u32 = 0x0010;
/// Win32 `SWP_SHOWWINDOW`.
pub const WIN32_SWP_SHOWWINDOW: u32 = 0x0040;
/// Win32 `SWP_HIDEWINDOW`.
pub const WIN32_SWP_HIDEWINDOW: u32 = 0x0080;
/// Win32 `HWND_TOPMOST` (`InsertAfter`).
pub const WIN32_HWND_TOPMOST: isize = -1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlaySetWindowPos {
    pub topmost: bool,
    pub x: i32,
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub flags: u32,
}

impl OverlaySetWindowPos {
    fn flags(hide: bool, show: bool, no_move: bool) -> u32 {
        let mut flags = WIN32_SWP_NOACTIVATE | WIN32_SWP_NOSIZE;
        if hide {
            flags |= WIN32_SWP_HIDEWINDOW;
        }
        if show {
            flags |= WIN32_SWP_SHOWWINDOW;
        }
        if no_move {
            flags |= WIN32_SWP_NOMOVE;
        }
        flags
    }

    pub fn hides(self) -> bool {
        self.flags & WIN32_SWP_HIDEWINDOW != 0
    }

    pub fn shows(self) -> bool {
        self.flags & WIN32_SWP_SHOWWINDOW != 0
    }

    pub fn activates(self) -> bool {
        self.flags & WIN32_SWP_NOACTIVATE == 0
    }

    pub fn changes_size(self) -> bool {
        self.flags & WIN32_SWP_NOSIZE == 0
    }

    pub fn changes_origin(self) -> bool {
        self.flags & WIN32_SWP_NOMOVE == 0
    }
}

/// `orderOut` equivalent: hide, keep last size and origin, stay topmost, do
/// not activate.
pub fn overlay_park_pos() -> OverlaySetWindowPos {
    OverlaySetWindowPos {
        topmost: true,
        x: 0,
        y: 0,
        cx: 0,
        cy: 0,
        flags: OverlaySetWindowPos::flags(true, false, true),
    }
}

/// Map without moving. Used when the overlay is already at the caret.
pub fn overlay_show_in_place_pos() -> OverlaySetWindowPos {
    OverlaySetWindowPos {
        topmost: true,
        x: 0,
        y: 0,
        cx: 0,
        cy: 0,
        flags: OverlaySetWindowPos::flags(false, true, true),
    }
}

/// Show at a Quartz/Win32 top-left origin. Width/height are ignored: GPUI
/// `window.resize` owns size, a second `SetWindowPos` size would fight it.
pub fn overlay_place_pos(x: f64, y: f64) -> OverlaySetWindowPos {
    OverlaySetWindowPos {
        topmost: true,
        x: x.round() as i32,
        y: y.round() as i32,
        cx: 0,
        cy: 0,
        flags: OverlaySetWindowPos::flags(false, true, false),
    }
}

/// Win32 `SetWindowPos` is top-left, same as overlay Quartz coords. Do not
/// apply the Cocoa Y flip used on macOS.
pub fn windows_overlay_applies_cocoa_y_flip() -> bool {
    false
}

/// Title lookup is the Linux/macOS fallback when a native handle is missing.
/// On Windows the GPUI HWND is required; a missing handle parks (returns
/// false) rather than walking windows by title or using a window rect.
pub fn windows_titled_overlay_places() -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayRawWindowKind {
    Win32,
    Other,
}

pub fn overlay_hwnd_bits(kind: OverlayRawWindowKind, hwnd: isize) -> Option<isize> {
    match kind {
        OverlayRawWindowKind::Win32 => Some(hwnd),
        OverlayRawWindowKind::Other => None,
    }
}

/// `GetSystemMetrics` virtual screen. Empty when width/height are unusable so
/// a caret without a screen list still parks (see `fig_desktop` overlay).
pub fn screens_from_virtual_metrics(x: i32, y: i32, width: i32, height: i32) -> Vec<(f64, f64, f64, f64)> {
    if width <= 0 || height <= 0 {
        Vec::new()
    } else {
        vec![(x as f64, y as f64, width as f64, height as f64)]
    }
}

/// `GetDeviceCaps(LOGPIXELSX) / 96`. Invalid or non-positive DPI is 1.0 so
/// placement does not NaN-clamp the list off-screen.
pub fn placement_scale_from_logpixelsx(dpi: i32) -> f64 {
    if dpi <= 0 { 1.0 } else { dpi as f64 / 96.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_flag_constants_match_the_sdk() {
        assert_eq!(WIN32_SWP_NOSIZE, 0x0001);
        assert_eq!(WIN32_SWP_NOMOVE, 0x0002);
        assert_eq!(WIN32_SWP_NOACTIVATE, 0x0010);
        assert_eq!(WIN32_SWP_SHOWWINDOW, 0x0040);
        assert_eq!(WIN32_SWP_HIDEWINDOW, 0x0080);
        assert_eq!(WIN32_HWND_TOPMOST, -1);
    }

    #[test]
    fn park_hides_without_activating_or_resizing() {
        let pos = overlay_park_pos();
        assert!(pos.topmost);
        assert!(pos.hides());
        assert!(!pos.shows());
        assert!(!pos.activates());
        assert!(!pos.changes_size());
        assert!(!pos.changes_origin());
        assert_eq!(
            pos.flags,
            WIN32_SWP_HIDEWINDOW | WIN32_SWP_NOACTIVATE | WIN32_SWP_NOSIZE | WIN32_SWP_NOMOVE
        );
    }

    #[test]
    fn place_shows_at_rounded_origin_and_leaves_size_to_gpui() {
        let pos = overlay_place_pos(10.6, -1.5);
        assert!(pos.topmost);
        assert!(pos.shows());
        assert!(!pos.hides());
        assert!(!pos.activates());
        assert!(!pos.changes_size());
        assert!(pos.changes_origin());
        assert_eq!(pos.x, 11);
        assert_eq!(pos.y, -2);
        assert_eq!(pos.cx, 0);
        assert_eq!(pos.cy, 0);
        assert_eq!(
            pos.flags,
            WIN32_SWP_SHOWWINDOW | WIN32_SWP_NOACTIVATE | WIN32_SWP_NOSIZE
        );
    }

    #[test]
    fn show_in_place_does_not_move_to_the_origin() {
        let pos = overlay_show_in_place_pos();
        assert!(pos.shows());
        assert!(!pos.hides());
        assert!(!pos.changes_origin());
        assert!(!pos.activates());
        assert!(!pos.changes_size());
        assert_ne!(pos, overlay_place_pos(0.0, 0.0), "show-in-place must keep SWP_NOMOVE");
    }

    #[test]
    fn windows_placement_is_top_left_and_needs_an_hwnd() {
        assert!(!windows_overlay_applies_cocoa_y_flip());
        assert!(!windows_titled_overlay_places());
        assert_eq!(overlay_hwnd_bits(OverlayRawWindowKind::Win32, 0x1234), Some(0x1234));
        assert_eq!(overlay_hwnd_bits(OverlayRawWindowKind::Other, 0x1234), None);
    }

    #[test]
    fn windows_y_flip_is_not_named_quartz() {
        let src = include_str!("windows.rs");
        assert!(
            src.contains("screen_y_to_frame_y"),
            "off-Mac Y flip must use the screen-space name"
        );
        assert!(!src.contains("quartz_y_"), "windows.rs must not export quartz_y_*");
        assert!(
            !src.contains("Fig Autocomplete"),
            "Windows overlay must not keep the Fig window name"
        );
        assert_eq!(crate::OVERLAY_WINDOW_TITLE, "Easy Complete");
    }

    #[test]
    fn virtual_screen_with_no_extent_is_not_a_placement_surface() {
        assert!(screens_from_virtual_metrics(0, 0, 0, 0).is_empty());
        assert!(screens_from_virtual_metrics(0, 0, 1920, 0).is_empty());
        assert!(screens_from_virtual_metrics(-100, 0, 0, 1080).is_empty());
        assert_eq!(
            screens_from_virtual_metrics(-1920, 0, 3840, 1080),
            vec![(-1920.0, 0.0, 3840.0, 1080.0)]
        );
    }

    #[test]
    fn dpi_scale_is_relative_to_96_and_rejects_junk() {
        assert_eq!(placement_scale_from_logpixelsx(96), 1.0);
        assert_eq!(placement_scale_from_logpixelsx(192), 2.0);
        assert_eq!(placement_scale_from_logpixelsx(0), 1.0);
        assert_eq!(placement_scale_from_logpixelsx(-96), 1.0);
    }
}
