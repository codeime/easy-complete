//! Windows overlay placement via the GPUI HWND. Size stays with `window.resize`.
//!
//! Flag/HWND/DPI policy lives in `windows_overlay` so Linux CI can pin F5.
//! This module is `cfg(windows)` and performs the `SetWindowPos` call.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, GetMonitorInfoW, LOGPIXELSX, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, HWND_TOPMOST, SET_WINDOW_POS_FLAGS, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SetWindowPos,
};

use crate::windows_overlay::{
    OverlayRawWindowKind, OverlaySetWindowPos, overlay_hwnd_bits, overlay_park_pos, overlay_place_pos,
    overlay_show_in_place_pos, placement_scale_from_logpixelsx, screens_from_virtual_metrics,
    windows_titled_overlay_places,
};

pub const OVERLAY_WINDOW_TITLE: &str = "Fig Autocomplete";

pub fn overlay_placement_scale() -> f64 {
    unsafe {
        let hdc = GetDC(HWND::default());
        if hdc.is_invalid() {
            return placement_scale_from_logpixelsx(0);
        }
        let dpi = GetDeviceCaps(hdc, LOGPIXELSX);
        let _ = ReleaseDC(HWND::default(), hdc);
        placement_scale_from_logpixelsx(dpi)
    }
}

pub fn invalidate_cached_overlay_x_window() {}

pub fn harden_overlay_window() {}

pub fn harden_overlay_window_titled(_title: &str) {}

pub fn polish_overlay_window_titled(_title: &str) {}

pub fn set_overlay_window_level(_level: i64) {}

pub fn set_overlay_window_level_for_title(_title: &str, _level: i64) {}

pub fn set_overlay_visible_titled(_title: &str, _visible: bool) {}

pub fn harden_overlay_window_handle(_window: &gpui::Window) {}

fn apply_pos(hwnd: HWND, pos: OverlaySetWindowPos) -> bool {
    debug_assert!(pos.topmost);
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            pos.x,
            pos.y,
            pos.cx,
            pos.cy,
            SET_WINDOW_POS_FLAGS(pos.flags),
        )
        .is_ok()
    }
}

pub fn park_overlay_window_handle(window: &gpui::Window) {
    if let Some(hwnd) = hwnd_from(window) {
        let _ = apply_pos(hwnd, overlay_park_pos());
    }
}

pub fn set_overlay_visible_handle(window: &gpui::Window, visible: bool) {
    if visible {
        if let Some(hwnd) = hwnd_from(window) {
            let _ = apply_pos(hwnd, overlay_show_in_place_pos());
        }
    } else {
        park_overlay_window_handle(window);
    }
}

pub fn set_overlay_frame_handle(window: &gpui::Window, x: f64, y: f64, width: f64, height: f64) -> bool {
    let _ = (width, height);
    let Some(hwnd) = hwnd_from(window) else {
        return false;
    };
    apply_pos(hwnd, overlay_place_pos(x, y))
}

pub fn park_overlay_window_titled(_title: &str) {}

pub fn quartz_y_to_cocoa_frame_y(quartz_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - quartz_y - height
}

pub fn overlay_screens() -> Vec<(f64, f64, f64, f64)> {
    unsafe {
        screens_from_virtual_metrics(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

pub fn set_overlay_frame_titled(_title: &str, _x: f64, _y: f64, _width: f64, _height: f64) -> bool {
    windows_titled_overlay_places()
}

pub fn system_appearance_is_dark() -> bool {
    false
}

fn hwnd_from(window: &gpui::Window) -> Option<HWND> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let (kind, bits) = match handle.as_raw() {
        RawWindowHandle::Win32(win32) => (OverlayRawWindowKind::Win32, win32.hwnd.get() as isize),
        _ => (OverlayRawWindowKind::Other, 0),
    };
    overlay_hwnd_bits(kind, bits).map(|bits| HWND(bits as _))
}

#[allow(dead_code)]
fn monitor_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info).ok()?;
        Some(info.rcWork)
    }
}
