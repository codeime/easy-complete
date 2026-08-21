//! Track the focused X11 window. Used to convert relative IBus caret coords
//! and to hide the overlay when focus leaves a supported terminal.

use std::sync::Arc;

use fig_util::consts::linux::DESKTOP_APP_WM_CLASS;
use fig_util::terminal::LINUX_TERMINALS;
use tracing::{debug, warn};
use x11rb::connection::Connection;
use x11rb::properties::WmClass;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, Property, Window};
use x11rb::rust_connection::RustConnection;

use super::{ActiveWindow, PlatformStateImpl};
use crate::EventLoopProxy;
use crate::event::{Event as HostEvent, WindowEvent};
use crate::webview::AUTOCOMPLETE_ID;

pub fn spawn(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) {
    std::thread::Builder::new()
        .name("ec-x11-focus".into())
        .spawn(move || {
            if let Err(err) = run(proxy, state) {
                warn!(%err, "X11 focus tracker exited");
            }
        })
        .ok();
}

fn run(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) -> anyhow::Result<()> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| anyhow::anyhow!("no X11 screen"))?
        .root;
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?
    .check()?;

    if let Err(err) = process_focus(&conn, root, &proxy, &state) {
        debug!(%err, "initial X11 focus probe skipped");
    }

    while let Ok(event) = conn.wait_for_event() {
        if let Event::PropertyNotify(notify) = event {
            if notify.state != Property::NEW_VALUE {
                continue;
            }
            let Ok(cookie) = conn.get_atom_name(notify.atom) else {
                continue;
            };
            let Ok(name) = cookie.reply() else {
                continue;
            };
            if name.name == b"_NET_ACTIVE_WINDOW" {
                if let Err(err) = process_focus(&conn, root, &proxy, &state) {
                    debug!(%err, "X11 focus update skipped");
                }
            }
        }
    }
    Ok(())
}

fn process_focus(
    conn: &RustConnection,
    root: Window,
    proxy: &EventLoopProxy,
    state: &PlatformStateImpl,
) -> anyhow::Result<()> {
    super::mark_wm_data();
    let window = ewmh_active_window(conn, root).unwrap_or(conn.get_input_focus()?.reply()?.focus);
    let Some((focused, wm_class, wm_instance)) = resolve_wm_class(conn, window)? else {
        return Ok(());
    };

    debug!(%wm_class, %wm_instance, window = focused, "X11 focus");

    if wm_class == DESKTOP_APP_WM_CLASS || wm_instance == DESKTOP_APP_WM_CLASS {
        return Ok(());
    }

    let terminal = LINUX_TERMINALS.iter().find(|terminal| {
        terminal.wm_class() == Some(wm_class.as_str())
            || terminal.wm_class() == Some(wm_instance.as_str())
            || terminal.wm_class_instance() == Some(wm_class.as_str())
            || terminal.wm_class_instance() == Some(wm_instance.as_str())
    });

    if terminal.is_none() {
        hide(proxy);
        *state.active_terminal.lock() = None;
        *state.active_window.lock() = None;
        *state.x11_classified.lock() = Some(false);
        return Ok(());
    }

    let geometry = client_root_geometry(conn, focused)?;
    *state.active_terminal.lock() = terminal.cloned();
    *state.active_window.lock() = Some(geometry);
    *state.x11_classified.lock() = Some(true);
    Ok(())
}

fn ui_scale(conn: &RustConnection, root: Window) -> f32 {
    if let Ok(var) = std::env::var("GPUI_X11_SCALE_FACTOR") {
        if let Ok(scale) = var.parse::<f32>() {
            if scale.is_finite() && scale > 0.0 {
                return scale;
            }
        }
    }
    xft_dpi_scale(conn, root).unwrap_or(1.0)
}

fn xft_dpi_scale(conn: &RustConnection, root: Window) -> Option<f32> {
    let reply = conn
        .get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 64 * 1024)
        .ok()?
        .reply()
        .ok()?;
    scale_from_xft_dpi_text(&String::from_utf8(reply.value).ok()?)
}

fn scale_from_xft_dpi_text(text: &str) -> Option<f32> {
    for line in text.split('\n') {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Xft.dpi") else {
            continue;
        };
        let rest = rest.trim_start_matches([':', ' ', '\t']);
        let dpi: f32 = rest.split_whitespace().next()?.parse().ok()?;
        let scale = dpi / 96.0;
        if scale.is_finite() && scale > 0.0 {
            return Some(scale);
        }
    }
    None
}

fn ewmh_active_window(conn: &RustConnection, root: Window) -> Option<Window> {
    let atom = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?.atom;
    let reply = conn
        .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    reply.value32()?.next().filter(|&window| window != 0)
}

fn resolve_wm_class(conn: &RustConnection, mut window: Window) -> anyhow::Result<Option<(Window, String, String)>> {
    for _ in 0..64 {
        // 0 = None, 1 = PointerRoot.
        if window <= 1 {
            return Ok(None);
        }
        let class = match WmClass::get(conn, window) {
            Ok(cookie) => cookie.reply().ok().flatten(),
            Err(_) => return Ok(None),
        };
        if let Some(class) = class {
            let wm_class = String::from_utf8_lossy(class.class()).into_owned();
            let wm_instance = String::from_utf8_lossy(class.instance()).into_owned();
            if wm_class != "FocusProxy" && (!wm_class.is_empty() || !wm_instance.is_empty()) {
                return Ok(Some((window, wm_class, wm_instance)));
            }
        }
        window = match conn.query_tree(window) {
            Ok(cookie) => match cookie.reply() {
                Ok(tree) => tree.parent,
                Err(_) => return Ok(None),
            },
            Err(_) => return Ok(None),
        };
    }
    Ok(None)
}

fn client_root_geometry(conn: &RustConnection, window: Window) -> anyhow::Result<ActiveWindow> {
    let geom = conn.get_geometry(window)?.reply()?;
    let root = conn.query_tree(window)?.reply()?.root;
    let translated = conn.translate_coordinates(window, root, 0, 0)?.reply()?;
    Ok(ActiveWindow {
        outer_x: i32::from(translated.dst_x),
        outer_y: i32::from(translated.dst_y),
        outer_width: i32::from(geom.width),
        outer_height: i32::from(geom.height),
        scale: ui_scale(conn, root),
    })
}

fn hide(proxy: &EventLoopProxy) {
    let _ = proxy.send_event(HostEvent::WindowEvent {
        window_id: AUTOCOMPLETE_ID,
        window_event: WindowEvent::Hide,
    });
}

#[cfg(test)]
mod tests {
    use super::scale_from_xft_dpi_text;

    #[test]
    fn xft_dpi_192_is_scale_two() {
        assert_eq!(scale_from_xft_dpi_text("Xft.dpi:\t192\nXft.antialias:\t1"), Some(2.0));
        assert_eq!(scale_from_xft_dpi_text("Xft.dpi: 96"), Some(1.0));
        assert_eq!(scale_from_xft_dpi_text(""), None);
    }
}
