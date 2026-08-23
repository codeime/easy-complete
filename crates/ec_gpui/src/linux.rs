//! Linux overlay window: X11 map/unmap and caret-relative configure.
//!
//! GPUI's 0.2.2 X11 `window_handle` is unimplemented, so the overlay is found
//! by title (same fallback macOS uses when the `NSWindow` pointer is missing).
//! Coordinates are X11 top-left, matching the overlay's top-left screen origin.
//!
//! Native Wayland has no placement API in GPUI 0.2.2 (no layer-shell).
//! GNOME Wayland still has XWayland (`DISPLAY`); the overlay uses that.
//! `overlay_screens` is empty without an X11 connection and the overlay parks.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tracing::debug;
use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::errors::ConnectionError;
use x11rb::protocol::randr;
use x11rb::protocol::xproto::{self, AtomEnum, ConfigureWindowAux, ConnectionExt, EventMask, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

use crate::overlay::OVERLAY_WINDOW_TITLE;

/// Coalesce place/probe bursts onto one display connection and one RandR list.
const X11_CACHE_TTL: Duration = Duration::from_millis(500);

type OverlayScreen = (f64, f64, f64, f64);
type CachedScreens = (Instant, Vec<OverlayScreen>);
type InternedAtoms = Vec<(Box<[u8]>, xproto::Atom)>;

static CACHED_X_WINDOW: AtomicU32 = AtomicU32::new(0);
static X11_CONNECTS: AtomicU32 = AtomicU32::new(0);
static DPI_SCALE: OnceLock<f64> = OnceLock::new();
static INTERNED_ATOMS: OnceLock<Mutex<InternedAtoms>> = OnceLock::new();
static X11: Mutex<X11Cache> = Mutex::new(X11Cache {
    display: None,
    retry_at: None,
    screens: None,
});

struct X11Display {
    conn: RustConnection,
    screen_num: usize,
}

struct X11Cache {
    display: Option<X11Display>,
    retry_at: Option<Instant>,
    screens: Option<CachedScreens>,
}

fn lock_x11() -> std::sync::MutexGuard<'static, X11Cache> {
    X11.lock().unwrap_or_else(|err| err.into_inner())
}

fn discard_display(cache: &mut X11Cache) {
    cache.display = None;
    cache.screens = None;
    cache.retry_at = Some(Instant::now() + X11_CACHE_TTL);
    CACHED_X_WINDOW.store(0, Ordering::Relaxed);
}

fn ensure_connected(cache: &mut X11Cache) -> bool {
    if cache.display.is_some() {
        return true;
    }
    if let Some(retry_at) = cache.retry_at {
        if Instant::now() < retry_at {
            return false;
        }
    }
    X11_CONNECTS.fetch_add(1, Ordering::Relaxed);
    match RustConnection::connect(None) {
        Ok((conn, screen_num)) => {
            cache.retry_at = None;
            // A previous failed probe may still hold an empty list inside the TTL.
            cache.screens = None;
            cache.display = Some(X11Display { conn, screen_num });
            true
        },
        Err(_) => {
            cache.retry_at = Some(Instant::now() + X11_CACHE_TTL);
            false
        },
    }
}

fn with_x11<R>(f: impl FnOnce(&RustConnection, usize) -> R) -> Option<R> {
    let mut cache = lock_x11();
    if !ensure_connected(&mut cache) {
        return None;
    }
    let result = {
        let display = cache.display.as_ref()?;
        f(&display.conn, display.screen_num)
    };
    // RustConnection has no Drop flush. Map+ABOVE used to race the implicit
    // close of a per-call connection; a cached one would hold the ClientMessage
    // in the write buffer until the next reply() on this conn.
    let flush_failed = cache
        .display
        .as_ref()
        .is_some_and(|display| display.conn.flush().is_err());
    if flush_failed {
        discard_display(&mut cache);
    }
    Some(result)
}

pub fn harden_overlay_window() {
    harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
}

pub fn harden_overlay_window_titled(title: &str) {
    let _ = with_x11(|conn, screen_num| {
        let Some(window) = overlay_x_window(conn, screen_num, title) else {
            return;
        };
        apply_overlay_hints(conn, screen_num, window);
    });
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
    let _ = with_x11(|conn, screen_num| {
        let Some(window) = overlay_x_window(conn, screen_num, title) else {
            return;
        };
        if !checked_void(conn.unmap_window(window)) {
            CACHED_X_WINDOW.store(0, Ordering::Relaxed);
        }
    });
}

pub fn invalidate_cached_overlay_x_window() {
    CACHED_X_WINDOW.store(0, Ordering::Relaxed);
}

pub fn set_overlay_frame_titled(title: &str, x: f64, y: f64, width: f64, height: f64) -> bool {
    // Size is GPUI's `window.resize`, which already multiplies by Xft DPI.
    // A second ConfigureWindow width/height on this connection would fight it.
    let _ = (width, height);
    with_x11(|conn, screen_num| {
        let Some(window) = overlay_x_window(conn, screen_num, title) else {
            return false;
        };
        apply_overlay_properties(conn, window);
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
        announce_overlay_above(conn, screen_num, window);
        true
    })
    .unwrap_or(false)
}

fn checked_void(cookie: Result<VoidCookie<'_, RustConnection>, ConnectionError>) -> bool {
    cookie.map(|cookie| cookie.check().is_ok()).unwrap_or(false)
}

pub fn screen_y_to_frame_y(screen_y: f64, height: f64, primary_origin_y: f64, primary_height: f64) -> f64 {
    primary_origin_y + primary_height - screen_y - height
}

/// X11 / XWayland outputs in top-left screen space. Empty when there is no
/// display connection, so a caret without screens still parks (see overlay).
///
/// Place/probe used to open a new `RustConnection` per call. One cached
/// connection plus a short TTL on this list means `layout_overlay` and
/// `apply_position` see the same outputs.
pub fn overlay_screens() -> Vec<(f64, f64, f64, f64)> {
    let mut cache = lock_x11();
    let cached = cache.screens.as_ref().and_then(|(fetched_at, screens)| {
        if fetched_at.elapsed() < X11_CACHE_TTL {
            Some((screens.clone(), cache.display.is_some()))
        } else {
            None
        }
    });
    if let Some((screens, has_display)) = cached {
        // A failed probe caches an empty list for the retry TTL. Do not keep
        // serving that once `with_x11` has actually connected.
        if !screens.is_empty() || !has_display {
            return screens;
        }
    }
    if !ensure_connected(&mut cache) {
        cache.screens = Some((Instant::now(), Vec::new()));
        return Vec::new();
    }
    let screens = {
        let Some(display) = cache.display.as_ref() else {
            cache.screens = Some((Instant::now(), Vec::new()));
            return Vec::new();
        };
        query_overlay_screens(&display.conn, display.screen_num)
    };
    cache.screens = Some((Instant::now(), screens.clone()));
    screens
}

fn query_overlay_screens(conn: &RustConnection, screen_num: usize) -> Vec<(f64, f64, f64, f64)> {
    let Some(root) = conn.setup().roots.get(screen_num).map(|s| s.root) else {
        return Vec::new();
    };
    if let Ok(screens) = randr_screens(conn, root) {
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
    if let Some(scale) = DPI_SCALE.get() {
        return *scale;
    }
    match xft_dpi_scale() {
        Some(scale) => {
            let _ = DPI_SCALE.set(scale);
            scale
        },
        None => 1.0,
    }
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
    with_x11(|conn, screen_num| -> Option<f64> {
        let root = conn.setup().roots.get(screen_num)?.root;
        let reply = conn
            .get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 64 * 1024)
            .ok()?
            .reply()
            .ok()?;
        scale_from_xft_dpi_text(&String::from_utf8(reply.value).ok()?)
    })
    .flatten()
}

fn overlay_x_window(conn: &RustConnection, screen_num: usize, title: &str) -> Option<xproto::Window> {
    let cached = CACHED_X_WINDOW.load(Ordering::Relaxed);
    if cached != 0 {
        // Trust the id until map/configure/unmap fails or the overlay is
        // recreated (`invalidate_cached_overlay_x_window`). Re-reading
        // WM_NAME on every place is a round-trip we already paid.
        return Some(cached);
    }
    let window = find_window_by_title(conn, screen_num, title)?;
    CACHED_X_WINDOW.store(window, Ordering::Relaxed);
    Some(window)
}

fn map_overlay_titled(title: &str) {
    let _ = with_x11(|conn, screen_num| {
        let Some(window) = overlay_x_window(conn, screen_num, title) else {
            return;
        };
        apply_overlay_properties(conn, window);
        if !checked_void(conn.map_window(window)) {
            CACHED_X_WINDOW.store(0, Ordering::Relaxed);
            return;
        }
        announce_overlay_above(conn, screen_num, window);
    });
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
    {
        let cache = interned_atoms();
        if let Some((_, atom)) = cache.iter().find(|(cached, _)| cached.as_ref() == name) {
            return Some(*atom);
        }
    }
    let atom = conn.intern_atom(false, name).ok()?.reply().ok()?.atom;
    let mut cache = interned_atoms();
    if !cache.iter().any(|(cached, _)| cached.as_ref() == name) {
        cache.push((name.to_vec().into_boxed_slice(), atom));
    }
    Some(atom)
}

fn interned_atoms() -> std::sync::MutexGuard<'static, InternedAtoms> {
    INTERNED_ATOMS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
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
    use super::{
        CACHED_X_WINDOW, OVERLAY_WINDOW_TITLE, Ordering, RustConnection, X11_CONNECTS, announce_overlay_above,
        apply_overlay_properties, harden_overlay_window_titled, intern, lock_x11, overlay_placement_scale,
        overlay_screens, park_overlay_window_titled, scale_from_xft_dpi_text, set_overlay_frame_titled,
    };
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, WindowClass};

    #[test]
    fn overlay_screens_is_the_non_mac_screen_list_name() {
        let src = include_str!("linux.rs");
        let start = src.find("pub fn overlay_screens()").expect("overlay_screens");
        let body = &src[start..];
        let end = body.find("#[cfg(test)]").unwrap_or(body.len());
        let body = &body[..end];
        assert!(body.contains("pub fn overlay_screens()"));
        assert!(
            !body.contains("screens_quartz"),
            "non-Mac screen list must not keep the Quartz name"
        );
        let production = src.split("#[cfg(test)]").next().expect("production");
        assert!(
            production.contains("screen_y_to_frame_y"),
            "off-Mac Y flip must use the screen-space name"
        );
        assert!(
            !production.contains("quartz_y_"),
            "Linux must not export quartz_y_* — that name is Cocoa-only"
        );
        assert!(
            production.contains("find_window_by_title(conn, screen_num, title)"),
            "GPUI 0.2.2 has no X11 window_handle; place/park find by title"
        );
        assert!(
            production.contains("OVERLAY_WINDOW_TITLE"),
            "find-by-title must use OVERLAY_WINDOW_TITLE, not a leftover Fig string"
        );
        assert!(
            !production.contains("Fig Autocomplete"),
            "Linux find-by-title must not keep the Fig window name"
        );
    }

    #[test]
    fn screen_y_to_frame_y_keeps_the_previous_flip() {
        assert_eq!(super::screen_y_to_frame_y(100.0, 140.0, 0.0, 900.0), 660.0);
        assert_eq!(super::screen_y_to_frame_y(100.0, 0.0, 0.0, 900.0), 800.0);
        assert_eq!(super::screen_y_to_frame_y(50.0, 20.0, 200.0, 800.0), 930.0);
    }

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

    fn x11_connect_count() -> u32 {
        X11_CONNECTS.load(Ordering::Relaxed)
    }

    fn reset_x11_cache() {
        let mut cache = lock_x11();
        cache.display = None;
        cache.retry_at = None;
        cache.screens = None;
        CACHED_X_WINDOW.store(0, Ordering::Relaxed);
    }

    #[test]
    fn rust_connection_connect_is_centralized() {
        let src = include_str!("linux.rs");
        let production = src.split("#[cfg(test)]").next().expect("production");
        assert_eq!(
            production.matches("RustConnection::connect").count(),
            1,
            "place/probe must share one connect site, not one connection per X11 helper"
        );
        assert!(
            production.contains("X11_CACHE_TTL"),
            "overlay_screens must TTL-cache the RandR list so layout and place share it"
        );
        let with_x11 = production.find("fn with_x11").expect("with_x11");
        let with_x11 = &production[with_x11..];
        let with_x11_end = with_x11.find("\npub fn ").unwrap_or(with_x11.len());
        assert!(
            with_x11[..with_x11_end].contains("flush"),
            "cached connections do not flush on drop; place/map must flush ABOVE ClientMessages"
        );
        assert!(
            production.contains("discard_display"),
            "a flush/IO failure must drop the cached display so the next place reconnects"
        );
    }

    #[test]
    fn overlay_trusts_cached_x_window_and_interns_once() {
        let src = include_str!("linux.rs");
        let production = src.split("#[cfg(test)]").next().expect("production");
        let harden = {
            let start = production.find("pub fn harden_overlay_window_titled").expect("harden");
            let rest = &production[start..];
            let end = rest.find("\npub fn ").unwrap_or(rest.len());
            &rest[..end]
        };
        assert!(
            harden.contains("overlay_x_window(conn, screen_num, title)"),
            "harden must reuse CACHED_X_WINDOW instead of walking the tree"
        );
        assert!(
            !harden.contains("find_window_by_title"),
            "harden must not find_window_by_title; overlay_x_window does that on a miss"
        );
        let overlay_x = {
            let start = production.find("fn overlay_x_window").expect("overlay_x_window");
            let rest = &production[start..];
            let end = rest.find("\nfn ").unwrap_or(rest.len());
            &rest[..end]
        };
        assert!(
            overlay_x.contains("CACHED_X_WINDOW.load"),
            "place/park/harden share overlay_x_window"
        );
        assert!(
            !overlay_x.contains("window_title"),
            "a non-zero CACHED_X_WINDOW must be trusted until invalidate/failure"
        );
        assert!(
            overlay_x.contains("find_window_by_title(conn, screen_num, title)"),
            "cache miss still finds by title"
        );
        assert!(
            production.contains("INTERNED_ATOMS") && production.contains("interned_atoms"),
            "intern must cache atoms instead of intern_atom per place"
        );
        assert!(
            production.contains("static DPI_SCALE: OnceLock") && production.contains("DPI_SCALE.get()"),
            "Xft dpi must be OnceLock'd after the first successful read"
        );
    }

    #[test]
    fn overlay_screens_and_place_reuse_one_x11_connection() {
        reset_x11_cache();
        let before = x11_connect_count();
        let first = overlay_screens();
        let after_first = x11_connect_count();
        let second = overlay_screens();
        let after_second = x11_connect_count();
        assert_eq!(first, second, "TTL overlay_screens must return the same list");
        assert_eq!(
            after_first, after_second,
            "second overlay_screens must not open another X11 connection"
        );
        assert!(
            after_first == before || after_first == before + 1,
            "warming the cache is at most one connect, got {before} -> {after_first}"
        );

        park_overlay_window_titled(OVERLAY_WINDOW_TITLE);
        let _ = set_overlay_frame_titled(OVERLAY_WINDOW_TITLE, 0.0, 0.0, 10.0, 10.0);
        harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);
        let _ = overlay_placement_scale();
        assert_eq!(
            x11_connect_count(),
            after_first,
            "place/park/harden/scale must reuse the cached (conn, screen_num)"
        );
    }
}
