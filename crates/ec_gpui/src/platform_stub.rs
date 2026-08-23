//! Overlay stand-ins for platforms without a native window backend.
//! Linux uses `linux.rs`, Windows uses `windows.rs`. Caret-less hosts keep
//! the list hidden. Do not invent a window-rect placement fallback.

#![allow(dead_code)]

pub fn harden_overlay_window() {}

pub fn harden_overlay_window_titled(_title: &str) {}

pub fn polish_overlay_window_titled(_title: &str) {}

pub fn set_overlay_window_level(_level: i64) {}

pub fn set_overlay_window_level_for_title(_title: &str, _level: i64) {}

pub fn set_overlay_visible_titled(_title: &str, _visible: bool) {}

pub fn harden_overlay_window_handle(_window: &gpui::Window) {}

pub fn park_overlay_window_handle(_window: &gpui::Window) {}

pub fn set_overlay_visible_handle(_window: &gpui::Window, _visible: bool) {}

pub fn invalidate_cached_overlay_x_window() {}

pub fn overlay_placement_scale() -> f64 {
    1.0
}

pub fn set_overlay_frame_handle(_window: &gpui::Window, _x: f64, _y: f64, _width: f64, _height: f64) -> bool {
    false
}

pub fn park_overlay_window_titled(_title: &str) {}

pub fn screen_y_to_frame_y(screen_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - screen_y - height
}

pub fn overlay_screens() -> Vec<(f64, f64, f64, f64)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn stub_overlay_screens_are_empty() {
        assert!(super::overlay_screens().is_empty());
        let src = include_str!("platform_stub.rs");
        let start = src.find("pub fn overlay_screens()").expect("overlay_screens");
        let body = &src[start..src.find("#[cfg(test)]").unwrap_or(src.len())];
        assert!(!body.contains("screens_quartz"));
        assert!(!body.contains("quartz_y_"));
        assert!(src.contains("screen_y_to_frame_y"));
    }

    #[test]
    fn screen_y_to_frame_y_keeps_the_previous_flip() {
        assert_eq!(super::screen_y_to_frame_y(100.0, 140.0, 0.0, 900.0), 660.0);
    }
}

pub fn set_overlay_frame_titled(_title: &str, _x: f64, _y: f64, _width: f64, _height: f64) {}

pub fn system_appearance_is_dark() -> bool {
    false
}
