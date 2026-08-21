//! Convert an IBus caret rectangle into overlay screen space.
//!
//! A terminal window rectangle is only used to turn *relative* IBus
//! coordinates into a caret. It is never a placement fallback: if there is
//! no usable caret, the overlay stays hidden.
//!
//! Callers live behind `cfg(target_os = "linux")`. The module is compiled on
//! every OS so the conversion tests run in macOS CI.

#![allow(dead_code)]

use fig_proto::local::caret_position_hook::Origin;
use fig_util::terminal::PositioningKind;
use tao::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Size};

/// Screen-space caret the overlay already consumes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretOnScreen {
    pub position: Position,
    pub size: Size,
    pub origin: Origin,
}

/// IBus emits `(0, 0, 0, 0)` when it has no cursor. Height ≤ 0 is the same
/// class of unusable rect the IPC caret hook already rejects.
pub fn ibus_rect_is_usable(x: i32, y: i32, width: i32, height: i32) -> bool {
    let _ = (x, y, width);
    height > 0
}

/// `SetCursorLocation` is already in screen space. Konsole reports logical
/// pixels; other Linux terminals report physical.
pub fn caret_from_ibus_absolute(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    kind: PositioningKind,
) -> Option<CaretOnScreen> {
    if !ibus_rect_is_usable(x, y, width, height) {
        return None;
    }
    let (position, size) = match kind {
        PositioningKind::Logical => (
            LogicalPosition::new(x as f64, y as f64).into(),
            LogicalSize::new(width as f64, height as f64).into(),
        ),
        PositioningKind::Physical => (
            PhysicalPosition::new(x, y).into(),
            PhysicalSize::new(width, height).into(),
        ),
    };
    Some(CaretOnScreen {
        position,
        size,
        origin: Origin::TopLeft,
    })
}

/// `SetCursorLocationRelative` is relative to the focused window's outer
/// top-left. IBus `y` is the bottom of the caret box, so height is subtracted
/// to get the overlay's top-left origin.
pub fn caret_from_ibus_relative(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    window_outer_x: i32,
    window_outer_y: i32,
    scale: f32,
) -> Option<CaretOnScreen> {
    if !ibus_rect_is_usable(x, y, width, height) {
        return None;
    }
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let abs_x = (x as f32 / scale).round() as i32 + window_outer_x;
    let abs_y = (y as f32 / scale).round() as i32 + window_outer_y - (height as f32 / scale).round() as i32;
    let abs_w = (width as f32 / scale).round() as i32;
    let abs_h = (height as f32 / scale).round() as i32;
    if abs_h <= 0 {
        return None;
    }
    Some(CaretOnScreen {
        position: LogicalPosition::new(abs_x as f64, abs_y as f64).into(),
        size: LogicalSize::new(abs_w as f64, abs_h as f64).into(),
        origin: Origin::TopLeft,
    })
}

/// AT-SPI `GetCharacterExtents(..., SCREEN)` is already a top-left screen rect.
/// Window `GetExtents` must not go through here — that is a window box, not a caret.
pub fn caret_from_atspi_extents(x: i32, y: i32, width: i32, height: i32) -> Option<CaretOnScreen> {
    caret_from_ibus_absolute(x, y, width, height, PositioningKind::Physical)
}

/// `AtspiRole` from at-spi2-core. Frame/Window are the toplevels we may
/// query for an IBus-relative origin; Application is the walk stop.
pub const ATSPI_ROLE_FRAME: u32 = 23;
pub const ATSPI_ROLE_WINDOW: u32 = 69;
pub const ATSPI_ROLE_APPLICATION: u32 = 75;

pub fn atspi_state_changed_is_focus_gained(kind: &str, detail1: i32) -> bool {
    kind == "focused" && detail1 != 0
}

pub fn atspi_is_self_app(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    lower == fig_util::consts::APP_PROCESS_NAME
        || lower == fig_util::consts::linux::DESKTOP_APP_WM_CLASS
        || lower == fig_util::PRODUCT_NAME.to_ascii_lowercase()
        || lower.contains("easy-complete")
}

/// Bottom-left caret origins need a primary-screen height to convert into the
/// overlay's top-left space. Top-left (IBus / X11) does not.
pub fn caret_origin_needs_screens(origin: Origin) -> bool {
    matches!(origin, Origin::BottomLeft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_ibus_rect_is_not_a_caret() {
        assert!(!ibus_rect_is_usable(0, 0, 0, 0));
        assert!(caret_from_ibus_absolute(0, 0, 0, 0, PositioningKind::Physical).is_none());
        assert!(caret_from_ibus_relative(0, 0, 0, 0, 10, 20, 1.0).is_none());
    }

    #[test]
    fn absolute_physical_caret_keeps_screen_space() {
        let caret = caret_from_ibus_absolute(100, 200, 8, 16, PositioningKind::Physical).unwrap();
        assert_eq!(caret.origin, Origin::TopLeft);
        match caret.position {
            Position::Physical(p) => {
                assert_eq!(p.x, 100);
                assert_eq!(p.y, 200);
            },
            other => panic!("expected physical position, got {other:?}"),
        }
        match caret.size {
            Size::Physical(s) => {
                assert_eq!(s.width, 8);
                assert_eq!(s.height, 16);
            },
            other => panic!("expected physical size, got {other:?}"),
        }
    }

    #[test]
    fn relative_caret_is_offset_from_the_window_not_used_as_the_window() {
        let caret = caret_from_ibus_relative(10, 40, 8, 16, 100, 200, 1.0).unwrap();
        match caret.position {
            Position::Logical(p) => {
                assert_eq!(p.x, 110.0);
                assert_eq!(p.y, 224.0);
            },
            other => panic!("expected logical position, got {other:?}"),
        }
    }

    #[test]
    fn relative_caret_divides_ibus_coords_by_window_scale() {
        let caret = caret_from_ibus_relative(20, 80, 16, 32, 100, 200, 2.0).unwrap();
        match caret.position {
            Position::Logical(p) => {
                assert_eq!(p.x, 110.0);
                assert_eq!(p.y, 224.0);
            },
            other => panic!("expected logical position, got {other:?}"),
        }
    }

    #[test]
    fn relative_caret_without_a_positive_scale_is_dropped() {
        assert!(caret_from_ibus_relative(10, 40, 8, 16, 100, 200, 0.0).is_none());
        assert!(caret_from_ibus_relative(10, 40, 8, 16, 100, 200, f32::NAN).is_none());
    }

    #[test]
    fn top_left_origin_does_not_need_a_screen_list() {
        assert!(!caret_origin_needs_screens(Origin::TopLeft));
        assert!(caret_origin_needs_screens(Origin::BottomLeft));
    }

    #[test]
    fn atspi_character_extents_are_a_physical_caret() {
        let caret = caret_from_atspi_extents(80, 90, 8, 18).unwrap();
        assert_eq!(caret.origin, Origin::TopLeft);
        match caret.position {
            Position::Physical(p) => {
                assert_eq!(p.x, 80);
                assert_eq!(p.y, 90);
            },
            other => panic!("expected physical position, got {other:?}"),
        }
        assert!(caret_from_atspi_extents(80, 90, 8, 0).is_none());
    }

    #[test]
    fn atspi_role_numbers_match_the_spec() {
        assert_eq!(ATSPI_ROLE_FRAME, 23);
        assert_eq!(ATSPI_ROLE_WINDOW, 69);
        assert_eq!(ATSPI_ROLE_APPLICATION, 75);
    }

    #[test]
    fn atspi_state_changed_only_reacts_to_focus_gained() {
        assert!(atspi_state_changed_is_focus_gained("focused", 1));
        assert!(!atspi_state_changed_is_focus_gained("focused", 0));
        assert!(!atspi_state_changed_is_focus_gained("checked", 1));
    }

    #[test]
    fn atspi_does_not_hide_for_our_overlay() {
        assert!(atspi_is_self_app("easy-complete"));
        assert!(atspi_is_self_app("Easy Complete"));
        assert!(!atspi_is_self_app("gnome-terminal-server"));
        assert!(!atspi_is_self_app("Firefox"));
    }
}
