//! Linux overlay window: X11 map/unmap and caret-relative configure.
//!
//! GPUI's 0.2.2 X11 `window_handle` is unimplemented, so the overlay is found
//! by title (same fallback macOS uses when the `NSWindow` pointer is missing).
//! Coordinates are X11 top-left, matching the overlay's Quartz-style origin.
//!
//! Native Wayland has no placement API in GPUI 0.2.2 (no layer-shell).
//! GNOME Wayland still has XWayland (`DISPLAY`); the overlay uses that.
//! `screens_quartz` is empty without an X11 connection and the overlay parks.

use std::sync::atomic::{AtomicU32, Ordering};

use tracing::debug;
use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::errors::ConnectionError;
use x11rb::protocol::randr;
use x11rb::protocol::xproto::{self, AtomEnum, ConfigureWindowAux, ConnectionExt, EventMask, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

pub const OVERLAY_WINDOW_TITLE: &str = "Fig Autocomplete";

static CACHED_X_WINDOW: AtomicU32 = AtomicU32::new(0);

pub fn harden_overlay_window() {
    harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
}

pub fn harden_overlay_window_titled(title: &str) {
    let Ok((conn, screen_num)) = RustConnection::connect(None) else {
        return;
    };
    let Some(window) = find_window_by_title(&conn, screen_num, title) else {
        return;
    };
    CACHED_X_WINDOW.store(window, Ordering::Relaxed);
    apply_overlay_hints(&conn, screen_num, window);
    let _ = conn.flush();
}

pub fn polish_overlay_window_titled(title: &str) {
    harden_overlay_window_titled(title);
}

pub fn set_overlay_window_level(_level: i64) {}

pub fn set_overlay_window_level_for_title(_title: &str, _level: i64) {}

pub fn set_overlay_visible_titled(title: &str, visible: bool) {
    if visible {
        map_overlay_titled(title);
    } else {
        park_overlay_window_titled(title);
    }
}

pub fn harden_overlay_window_handle(_window: &gpui::Window) {
    // gpui 0.2.2's X11 `window_handle` is unimplemented and panics. Title
    // lookup is the same fallback macOS uses when the NSWindow pointer is
    // missing.
    harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
}

pub fn park_overlay_window_handle(_window: &gpui::Window) {
    park_overlay_window_titled(OVERLAY_WINDOW_TITLE);
}

pub fn set_overlay_visible_handle(window: &gpui::Window, visible: bool) {
    let _ = window;
    set_overlay_visible_titled(OVERLAY_WINDOW_TITLE, visible);
}

pub fn set_overlay_frame_handle(window: &gpui::Window, x: f64, y: f64, width: f64, height: f64) -> bool {
    let _ = window;
    set_overlay_frame_titled(OVERLAY_WINDOW_TITLE, x, y, width, height)
}

pub fn park_overlay_window_titled(title: &str) {
    let Ok((conn, screen_num)) = RustConnection::connect(None) else {
        return;
    };
    let Some(window) = overlay_x_window(&conn, screen_num, title) else {
        return;
    };
    if !checked_void(conn.unmap_window(window)) {
        CACHED_X_WINDOW.store(0, Ordering::Relaxed);
    }
}

pub fn invalidate_cached_overlay_x_window() {
    CACHED_X_WINDOW.store(0, Ordering::Relaxed);
}

pub fn set_overlay_frame_titled(title: &str, x: f64, y: f64, width: f64, height: f64) -> bool {
    // Size is GPUI's `window.resize`, which already multiplies by Xft DPI.
    // A second ConfigureWindow width/height on this connection would fight it.
    let _ = (width, height);
    let Ok((conn, screen_num)) = RustConnection::connect(None) else {
        return false;
    };
    let Some(window) = overlay_x_window(&conn, screen_num, title) else {
        return false;
    };
    apply_overlay_properties(&conn, window);
    let aux = ConfigureWindowAux::new().x(x.round() as i32).y(y.round() as i32);
    if !checked_void(conn.configure_window(window, &aux)) {
        CACHED_X_WINDOW.store(0, Ordering::Relaxed);
        return false;
    }
    if !checked_void(conn.map_window(window)) {
        CACHED_X_WINDOW.store(0, Ordering::Relaxed);
        return false;
    }
    // `_NET_WM_STATE` ClientMessage is for mapped windows. xfwm4 ignores it
    // on an unmapped client and would otherwise drop ABOVE.
    announce_overlay_above(&conn, screen_num, window);
    true
}

fn checked_void(cookie: Result<VoidCookie<'_, RustConnection>, ConnectionError>) -> bool {
    cookie.map(|cookie| cookie.check().is_ok()).unwrap_or(false)
}

pub fn quartz_y_to_cocoa_frame_y(quartz_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - quartz_y - height
}

/// X11 / XWayland outputs in top-left screen space. Empty when there is no
/// display connection, so a caret without screens still parks (see overlay).
pub fn screens_quartz() -> Vec<(f64, f64, f64, f64)> {
    let Ok((conn, screen_num)) = RustConnection::connect(None) else {
        return Vec::new();
    };
    let root = conn.setup().roots.get(screen_num).map(|s| s.root);
    let Some(root) = root else {
        return Vec::new();
    };
    if let Ok(screens) = randr_screens(&conn, root) {
        if !screens.is_empty() {
            return screens;
        }
    }
    conn.setup()
        .roots
        .get(screen_num)
        .map(|screen| vec![(0.0, 0.0, screen.width_in_pixels as f64, screen.height_in_pixels as f64)])
        .unwrap_or_default()
}

pub fn system_appearance_is_dark() -> bool {
    false
}

/// Device pixels per GPUI pixel. Overlay flip/clamp uses RandR physical
/// edges, so logical list size must be multiplied by this before placement.
pub fn overlay_placement_scale() -> f64 {
    if let Ok(var) = std::env::var("GPUI_X11_SCALE_FACTOR") {
        if let Ok(scale) = var.parse::<f64>() {
            if scale.is_finite() && scale > 0.0 {
                return scale;
            }
        }
    }
    xft_dpi_scale().unwrap_or(1.0)
}

fn scale_from_xft_dpi_text(text: &str) -> Option<f64> {
    for line in text.split('\n') {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Xft.dpi") else {
            continue;
        };
        let rest = rest.trim_start_matches([':', ' ', '\t']);
        let dpi: f64 = rest.split_whitespace().next()?.parse().ok()?;
        let scale = dpi / 96.0;
        if scale.is_finite() && scale > 0.0 {
            return Some(scale);
        }
    }
    None
}

fn xft_dpi_scale() -> Option<f64> {
    let (conn, screen_num) = RustConnection::connect(None).ok()?;
    let root = conn.setup().roots.get(screen_num)?.root;
    let reply = conn
        .get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 64 * 1024)
        .ok()?
        .reply()
        .ok()?;
    scale_from_xft_dpi_text(&String::from_utf8(reply.value).ok()?)
}

fn overlay_x_window(conn: &RustConnection, screen_num: usize, title: &str) -> Option<xproto::Window> {
    let cached = CACHED_X_WINDOW.load(Ordering::Relaxed);
    if cached != 0 && window_title(conn, cached).as_deref() == Some(title) {
        return Some(cached);
    }
    CACHED_X_WINDOW.store(0, Ordering::Relaxed);
    let window = find_window_by_title(conn, screen_num, title)?;
    CACHED_X_WINDOW.store(window, Ordering::Relaxed);
    Some(window)
}

fn map_overlay_titled(title: &str) {
    let Ok((conn, screen_num)) = RustConnection::connect(None) else {
        return;
    };
    let Some(window) = overlay_x_window(&conn, screen_num, title) else {
        return;
    };
    apply_overlay_properties(&conn, window);
    if !checked_void(conn.map_window(window)) {
        CACHED_X_WINDOW.store(0, Ordering::Relaxed);
        return;
    }
    announce_overlay_above(&conn, screen_num, window);
}

fn apply_overlay_hints(conn: &RustConnection, screen_num: usize, window: xproto::Window) {
    apply_overlay_properties(conn, window);
    announce_overlay_above(conn, screen_num, window);
}

fn apply_overlay_properties(conn: &RustConnection, window: xproto::Window) {
    let hints = crate::linux_overlay::overlay_x11_hints();
    debug_assert!(!hints.sends_active_window);
    if let (Some(type_atom), Some(notification)) = (
        intern(conn, crate::linux_overlay::NET_WM_WINDOW_TYPE.as_bytes()),
        intern(conn, hints.window_type.as_bytes()),
    ) {
        let _ = conn.change_property32(PropMode::REPLACE, window, type_atom, AtomEnum::ATOM, &[notification]);
    }
    let Some(net_wm_state) = intern(conn, crate::linux_overlay::NET_WM_STATE.as_bytes()) else {
        return;
    };
    let mut state_atoms = Vec::new();
    for name in hints.state {
        match intern(conn, name.as_bytes()) {
            Some(atom) => state_atoms.push(atom),
            None => return,
        }
    }
    let _ = conn.change_property32(PropMode::REPLACE, window, net_wm_state, AtomEnum::ATOM, &state_atoms);
}

fn announce_overlay_above(conn: &RustConnection, screen_num: usize, window: xproto::Window) {
    let Some(net_wm_state) = intern(conn, crate::linux_overlay::NET_WM_STATE.as_bytes()) else {
        return;
    };
    let Some(root) = conn.setup().roots.get(screen_num).map(|s| s.root) else {
        return;
    };
    let above = intern(conn, crate::linux_overlay::NET_WM_STATE_ABOVE.as_bytes()).unwrap_or(0);
    let skip_taskbar = intern(conn, crate::linux_overlay::NET_WM_STATE_SKIP_TASKBAR.as_bytes()).unwrap_or(0);
    let event = xproto::ClientMessageEvent::new(
        32,
        window,
        net_wm_state,
        [crate::linux_overlay::NET_WM_STATE_ADD, above, skip_taskbar, 0, 0],
    );
    let _ = conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    );
    debug!(window, "applied overlay X11 hints");
}

fn intern(conn: &RustConnection, name: &[u8]) -> Option<xproto::Atom> {
    conn.intern_atom(false, name).ok()?.reply().ok().map(|reply| reply.atom)
}

fn randr_screens(conn: &RustConnection, root: xproto::Window) -> anyhow::Result<Vec<(f64, f64, f64, f64)>> {
    let _ = randr::query_version(conn, 1, 5)?.reply()?;
    let reply = randr::get_monitors(conn, root, true)?.reply()?;
    let mut screens = Vec::new();
    let mut primary = None;
    for monitor in reply.monitors {
        let rect = (
            monitor.x as f64,
            monitor.y as f64,
            monitor.width as f64,
            monitor.height as f64,
        );
        if monitor.primary {
            primary = Some(rect);
        } else {
            screens.push(rect);
        }
    }
    if let Some(primary) = primary {
        screens.insert(0, primary);
    }
    Ok(screens)
}

fn find_window_by_title(conn: &RustConnection, screen_num: usize, title: &str) -> Option<xproto::Window> {
    let root = conn.setup().roots.get(screen_num)?.root;
    let mut stack = vec![root];
    while let Some(window) = stack.pop() {
        if window_title(conn, window).as_deref() == Some(title) {
            return Some(window);
        }
        if let Ok(cookie) = conn.query_tree(window) {
            if let Ok(tree) = cookie.reply() {
                stack.extend(tree.children);
            }
        }
    }
    None
}

fn window_title(conn: &RustConnection, window: xproto::Window) -> Option<String> {
    let reply = conn
        .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    String::from_utf8(reply.value).ok()
}

#[cfg(test)]
mod tests {
    use super::{RustConnection, announce_overlay_above, apply_overlay_properties, intern, scale_from_xft_dpi_text};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, WindowClass};

    #[test]
    fn xft_dpi_192_is_scale_two() {
        assert_eq!(scale_from_xft_dpi_text("Xft.dpi:\t192\nXft.antialias:\t1"), Some(2.0));
        assert_eq!(scale_from_xft_dpi_text("Xft.dpi: 96"), Some(1.0));
        assert_eq!(scale_from_xft_dpi_text(""), None);
    }

    fn property_atoms(conn: &RustConnection, window: xproto::Window, name: &[u8]) -> Vec<xproto::Atom> {
        let atom = intern(conn, name).expect("intern");
        let reply = conn
            .get_property(false, window, atom, AtomEnum::ATOM, 0, 16)
            .unwrap()
            .reply()
            .unwrap();
        reply.value32().map(Iterator::collect).unwrap_or_default()
    }

    #[test]
    fn overlay_hints_are_notification_above_and_do_not_steal_focus() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skip: no DISPLAY");
            return;
        }
        let (conn, screen_num) = RustConnection::connect(None).expect("x11");
        let screen = conn.setup().roots.get(screen_num).expect("screen");
        let root = screen.root;
        let visual = screen.root_visual;
        let focused = conn.generate_id().expect("focused id");
        let overlay = conn.generate_id().expect("overlay id");
        let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            focused,
            root,
            20,
            20,
            80,
            80,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &aux,
        )
        .unwrap()
        .check()
        .unwrap();
        conn.map_window(focused).unwrap().check().unwrap();
        conn.set_input_focus(xproto::InputFocus::POINTER_ROOT, focused, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .ok();
        conn.flush().ok();

        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            overlay,
            root,
            40,
            40,
            60,
            60,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &aux,
        )
        .unwrap()
        .check()
        .unwrap();
        apply_overlay_properties(&conn, overlay);
        conn.map_window(overlay).unwrap().check().unwrap();
        announce_overlay_above(&conn, screen_num, overlay);
        conn.flush().ok();
        std::thread::sleep(std::time::Duration::from_millis(150));

        let hints = crate::linux_overlay::overlay_x11_hints();
        let types = property_atoms(&conn, overlay, crate::linux_overlay::NET_WM_WINDOW_TYPE.as_bytes());
        let notification = intern(&conn, hints.window_type.as_bytes()).expect("notification atom");
        assert_eq!(types, vec![notification], "overlay must be {}", hints.window_type);
        let states = property_atoms(&conn, overlay, crate::linux_overlay::NET_WM_STATE.as_bytes());
        let above = intern(&conn, crate::linux_overlay::NET_WM_STATE_ABOVE.as_bytes()).expect("above");
        assert!(
            states.contains(&above),
            "overlay must be _NET_WM_STATE_ABOVE, got {states:?}"
        );

        let focus = conn.get_input_focus().unwrap().reply().unwrap();
        assert_ne!(focus.focus, overlay, "mapping the overlay must not steal input focus");

        let _ = conn.destroy_window(overlay);
        let _ = conn.destroy_window(focused);
        let _ = conn.flush();
    }
}
