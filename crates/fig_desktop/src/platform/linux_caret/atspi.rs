//! GNOME Wayland caret via AT-SPI character extents (the same bus Orca uses).
//!
//! GPUI 0.2.2 has no layer-shell, so the overlay still sits on XWayland.
//! Native Wayland terminals do not show up in X11 `_NET_ACTIVE_WINDOW`; their
//! caret is this path. A window `GetExtents` box is never treated as a caret.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};
use zbus::fdo::DBusProxy;
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedObjectPath;
use zbus::{MatchRule, Message, MessageStream};

use super::PlatformStateImpl;
use crate::EventLoopProxy;
use crate::event::{Event, WindowEvent, WindowPosition};
use crate::platform::caret::{
    ATSPI_ROLE_APPLICATION, ATSPI_ROLE_FRAME, ATSPI_ROLE_WINDOW, atspi_state_changed_is_focus_gained,
    caret_from_atspi_extents,
};
use crate::webview::AUTOCOMPLETE_ID;

const COORD_TYPE_SCREEN: u32 = 0;

pub fn spawn(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) {
    tokio::spawn(async move {
        loop {
            match listen(proxy.clone(), state.clone()).await {
                Ok(()) => debug!("AT-SPI caret listener ended"),
                Err(err) => warn!(%err, "AT-SPI caret listener failed"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn listen(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) -> anyhow::Result<()> {
    let session = zbus::Connection::session().await?;
    let address: String = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .await?
        .body()
        .deserialize()?;
    let conn = zbus::connection::Builder::address(address.as_str())?.build().await?;

    for event in ["object:text-caret-moved", "object:state-changed:focused"] {
        let _ = conn
            .call_method(
                Some("org.a11y.atspi.Registry"),
                "/org/a11y/atspi/registry",
                Some("org.a11y.atspi.Registry"),
                "RegisterEvent",
                &event,
            )
            .await;
    }

    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface("org.a11y.atspi.Event.Object")?
        .build();
    DBusProxy::new(&conn).await?.add_match_rule(rule).await?;

    use futures::StreamExt;
    let mut stream = MessageStream::from(conn.clone());
    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                warn!(%err, "AT-SPI message stream error");
                break;
            },
        };
        if msg.message_type() != MessageType::Signal {
            continue;
        }
        let header = msg.header();
        let Some(interface) = header.interface() else {
            continue;
        };
        if interface.as_str() != "org.a11y.atspi.Event.Object" {
            continue;
        }
        let Some(member) = header.member() else {
            continue;
        };
        match member.as_str() {
            "TextCaretMoved" | "StateChanged" => {
                if let Err(err) = handle_accessible(&conn, &msg, &proxy, &state).await {
                    debug!(%err, "AT-SPI caret probe skipped");
                }
            },
            _ => {},
        }
    }
    Ok(())
}

async fn handle_accessible(
    conn: &zbus::Connection,
    msg: &Message,
    proxy: &EventLoopProxy,
    state: &PlatformStateImpl,
) -> anyhow::Result<()> {
    let header = msg.header();
    let Some(sender) = header.sender() else {
        return Ok(());
    };
    let Some(path) = header.path() else {
        return Ok(());
    };
    let dest = sender.as_str();
    let path = path.as_str();
    let member = header.member().map(|m| m.as_str()).unwrap_or("");

    if member == "StateChanged" {
        let Some((kind, detail1)) = state_changed_focus(msg) else {
            return Ok(());
        };
        if !atspi_state_changed_is_focus_gained(&kind, detail1) {
            return Ok(());
        }
        *state.atspi_focused.lock() = Some((dest.to_owned(), path.to_owned()));
    } else if member == "TextCaretMoved" {
        let focused = state.atspi_focused.lock();
        if let Some((d, p)) = focused.as_ref() {
            if d != dest || p != path {
                return Ok(());
            }
        }
    }

    // IBus+X11 own the caret only while an X11 terminal is focused.
    if *state.x11_classified.lock() == Some(true) {
        return Ok(());
    }

    let Ok(app_name) = application_name(conn, dest, path).await else {
        return Ok(());
    };
    if crate::platform::caret::atspi_is_self_app(&app_name) {
        return Ok(());
    }
    let Some(terminal) = fig_util::Terminal::from_linux_identity(&app_name) else {
        if member == "StateChanged" {
            hide(proxy);
            *state.active_terminal.lock() = None;
            *state.active_window.lock() = None;
        }
        return Ok(());
    };

    *state.active_terminal.lock() = Some(terminal);
    if member == "TextCaretMoved" {
        let mut focused = state.atspi_focused.lock();
        if focused.is_none() {
            *focused = Some((dest.to_owned(), path.to_owned()));
        }
    }
    if let Some(window) = window_extents(conn, dest, path).await {
        *state.active_window.lock() = Some(window);
    }

    let offset: i32 = conn
        .call_method(Some(dest), path, Some("org.a11y.atspi.Text"), "GetCaretOffset", &())
        .await?
        .body()
        .deserialize()?;
    let (x, y, w, h): (i32, i32, i32, i32) = conn
        .call_method(
            Some(dest),
            path,
            Some("org.a11y.atspi.Text"),
            "GetCharacterExtents",
            &(offset, COORD_TYPE_SCREEN),
        )
        .await?
        .body()
        .deserialize()?;
    let Some(caret) = caret_from_atspi_extents(x, y, w, h) else {
        return Ok(());
    };
    send_caret(proxy, caret);
    Ok(())
}

async fn application_name(conn: &zbus::Connection, dest: &str, path: &str) -> anyhow::Result<String> {
    let (name, app_path): (String, OwnedObjectPath) = conn
        .call_method(
            Some(dest),
            path,
            Some("org.a11y.atspi.Accessible"),
            "GetApplication",
            &(),
        )
        .await?
        .body()
        .deserialize()?;
    if !name.is_empty() && fig_util::Terminal::from_linux_identity(&name).is_some() {
        return Ok(name);
    }
    let app_name: String = conn
        .call_method(
            Some(dest),
            app_path.as_str(),
            Some("org.a11y.atspi.Accessible"),
            "GetName",
            &(),
        )
        .await?
        .body()
        .deserialize()?;
    if !app_name.is_empty() {
        return Ok(app_name);
    }
    Ok(name)
}

async fn window_extents(conn: &zbus::Connection, dest: &str, start_path: &str) -> Option<super::ActiveWindow> {
    let mut current = start_path.to_owned();
    for _ in 0..32 {
        let role: u32 = conn
            .call_method(
                Some(dest),
                current.as_str(),
                Some("org.a11y.atspi.Accessible"),
                "GetRole",
                &(),
            )
            .await
            .ok()?
            .body()
            .deserialize()
            .ok()?;
        if role == ATSPI_ROLE_FRAME || role == ATSPI_ROLE_WINDOW {
            let (x, y, w, h): (i32, i32, i32, i32) = conn
                .call_method(
                    Some(dest),
                    current.as_str(),
                    Some("org.a11y.atspi.Component"),
                    "GetExtents",
                    &COORD_TYPE_SCREEN,
                )
                .await
                .ok()?
                .body()
                .deserialize()
                .ok()?;
            return Some(super::ActiveWindow {
                outer_x: x,
                outer_y: y,
                outer_width: w,
                outer_height: h,
                scale: 1.0,
            });
        }
        if role == ATSPI_ROLE_APPLICATION {
            return None;
        }
        let (_name, parent): (String, OwnedObjectPath) = conn
            .call_method(
                Some(dest),
                current.as_str(),
                Some("org.a11y.atspi.Accessible"),
                "GetParent",
                &(),
            )
            .await
            .ok()?
            .body()
            .deserialize()
            .ok()?;
        current = parent.as_str().to_owned();
    }
    None
}

fn state_changed_focus(msg: &Message) -> Option<(String, i32)> {
    let (kind, detail1, _detail2, _any, _props): (
        String,
        i32,
        i32,
        zbus::zvariant::OwnedValue,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) = msg.body().deserialize().ok()?;
    Some((kind, detail1))
}

fn hide(proxy: &EventLoopProxy) {
    let _ = proxy.send_event(Event::WindowEvent {
        window_id: AUTOCOMPLETE_ID,
        window_event: WindowEvent::Hide,
    });
}

fn send_caret(proxy: &EventLoopProxy, caret: crate::platform::caret::CaretOnScreen) {
    super::mark_wm_data();
    let _ = proxy.send_event(Event::WindowEvent {
        window_id: AUTOCOMPLETE_ID,
        window_event: WindowEvent::UpdateWindowGeometry {
            position: Some(WindowPosition::RelativeToCaret {
                caret_position: caret.position,
                caret_size: caret.size,
                origin: caret.origin,
            }),
            size: None,
            anchor: None,
            tx: None,
            dry_run: false,
        },
    });
}
