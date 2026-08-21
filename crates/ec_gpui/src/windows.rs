//! Windows overlay placement via the GPUI HWND. Size stays with `window.resize`.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, GetMonitorInfoW, LOGPIXELSX, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, HWND_TOPMOST, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowPos,
};

pub const OVERLAY_WINDOW_TITLE: &str = "Fig Autocomplete";

pub fn overlay_placement_scale() -> f64 {
    unsafe {
        let hdc = GetDC(HWND::default());
        if hdc.is_invalid() {
            return 1.0;
        }
        let dpi = GetDeviceCaps(hdc, LOGPIXELSX);
        let _ = ReleaseDC(HWND::default(), hdc);
        if dpi <= 0 { 1.0 } else { dpi as f64 / 96.0 }
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

pub fn park_overlay_window_handle(window: &gpui::Window) {
    if let Some(hwnd) = hwnd_from(window) {
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_HIDEWINDOW | SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE,
            );
        }
    }
}

pub fn set_overlay_visible_handle(window: &gpui::Window, visible: bool) {
    if visible {
        let _ = set_overlay_frame_handle(window, 0.0, 0.0, 1.0, 1.0);
    } else {
        park_overlay_window_handle(window);
    }
}

pub fn set_overlay_frame_handle(window: &gpui::Window, x: f64, y: f64, width: f64, height: f64) -> bool {
    let _ = (width, height);
    let Some(hwnd) = hwnd_from(window) else {
        return false;
    };
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x.round() as i32,
            y.round() as i32,
            0,
            0,
            SWP_SHOWWINDOW | SWP_NOACTIVATE | SWP_NOSIZE,
        )
        .is_ok()
    }
}

pub fn park_overlay_window_titled(_title: &str) {}

pub fn quartz_y_to_cocoa_frame_y(quartz_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - quartz_y - height
}

pub fn screens_quartz() -> Vec<(f64, f64, f64, f64)> {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN) as f64;
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN) as f64;
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64;
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64;
        if width <= 0.0 || height <= 0.0 {
            Vec::new()
        } else {
            vec![(x, y, width, height)]
        }
    }
}

pub fn set_overlay_frame_titled(_title: &str, _x: f64, _y: f64, _width: f64, _height: f64) -> bool {
    false
}

pub fn system_appearance_is_dark() -> bool {
    false
}

fn hwnd_from(window: &gpui::Window) -> Option<HWND> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as _)),
        _ => None,
    }
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
