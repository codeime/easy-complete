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
use crate::bootstrap::AUTOCOMPLETE_ID;
use crate::event::{Event, WindowEvent, WindowPosition};
use crate::platform::caret::{
    ATSPI_COORD_TYPE_SCREEN, ATSPI_IFACE_ACCESSIBLE, ATSPI_IFACE_TEXT, ATSPI_METHOD_GET_CARET_OFFSET,
    ATSPI_METHOD_GET_NAME, ATSPI_METHOD_GET_PARENT, ATSPI_PROP_CARET_OFFSET, ATSPI_PROP_NAME, ATSPI_PROP_PARENT,
    ATSPI_ROLE_APPLICATION, ATSPI_ROLE_FRAME, ATSPI_ROLE_WINDOW, atspi_state_changed_is_focus_gained,
    atspi_yields_to_ibus, caret_from_atspi_extents,
};

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
    let member = header.member().map_or("", |m| m.as_str());

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

    // D2: IBus+X11 own the caret while an X11 terminal is focused *and*
    // IBus is subscribed. If IBus never got SetCursorLocation (no
    // Monitoring, eavesdrop failed), keep probing AT-SPI extents.
    if atspi_yields_to_ibus(
        *state.x11_classified.lock(),
        state.ibus_listening.load(std::sync::atomic::Ordering::Acquire),
    ) {
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

    let offset = caret_offset(conn, dest, path).await?;
    let (x, y, w, h): (i32, i32, i32, i32) = conn
        .call_method(
            Some(dest),
            path,
            Some(ATSPI_IFACE_TEXT),
            "GetCharacterExtents",
            &(offset, ATSPI_COORD_TYPE_SCREEN),
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

async fn dbus_property<T>(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    interface: &str,
    name: &str,
) -> anyhow::Result<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
    T::Error: std::fmt::Display,
{
    let reply = conn
        .call_method(
            Some(dest),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, name),
        )
        .await?;
    let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
    T::try_from(value).map_err(|err| anyhow::anyhow!("AT-SPI {interface}.{name}: {err}"))
}

async fn property_or_method<T>(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    interface: &str,
    property: &str,
    method: &str,
) -> anyhow::Result<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue> + serde::de::DeserializeOwned + zbus::zvariant::Type,
    T::Error: std::fmt::Display,
{
    match dbus_property(conn, dest, path, interface, property).await {
        Ok(value) => Ok(value),
        Err(prop_err) => {
            debug!(%prop_err, interface, property, method, "AT-SPI property missing; trying method");
            let reply = conn.call_method(Some(dest), path, Some(interface), method, &()).await?;
            Ok(reply.body().deserialize()?)
        },
    }
}

pub(crate) async fn caret_offset(conn: &zbus::Connection, dest: &str, path: &str) -> anyhow::Result<i32> {
    property_or_method(
        conn,
        dest,
        path,
        ATSPI_IFACE_TEXT,
        ATSPI_PROP_CARET_OFFSET,
        ATSPI_METHOD_GET_CARET_OFFSET,
    )
    .await
}

pub(crate) async fn accessible_name(conn: &zbus::Connection, dest: &str, path: &str) -> anyhow::Result<String> {
    property_or_method(
        conn,
        dest,
        path,
        ATSPI_IFACE_ACCESSIBLE,
        ATSPI_PROP_NAME,
        ATSPI_METHOD_GET_NAME,
    )
    .await
}

pub(crate) async fn parent_ref(conn: &zbus::Connection, dest: &str, path: &str) -> Option<(String, OwnedObjectPath)> {
    if let Ok(pair) =
        dbus_property::<(String, OwnedObjectPath)>(conn, dest, path, ATSPI_IFACE_ACCESSIBLE, ATSPI_PROP_PARENT).await
    {
        return Some(pair);
    }
    conn.call_method(
        Some(dest),
        path,
        Some(ATSPI_IFACE_ACCESSIBLE),
        ATSPI_METHOD_GET_PARENT,
        &(),
    )
    .await
    .ok()?
    .body()
    .deserialize()
    .ok()
}

async fn application_name(conn: &zbus::Connection, dest: &str, path: &str) -> anyhow::Result<String> {
    let (name, app_path): (String, OwnedObjectPath) = conn
        .call_method(Some(dest), path, Some(ATSPI_IFACE_ACCESSIBLE), "GetApplication", &())
        .await?
        .body()
        .deserialize()?;
    if !name.is_empty() && fig_util::Terminal::from_linux_identity(&name).is_some() {
        return Ok(name);
    }
    let app_name = accessible_name(conn, dest, app_path.as_str()).await?;
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
                Some(ATSPI_IFACE_ACCESSIBLE),
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
                    &ATSPI_COORD_TYPE_SCREEN,
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
        let (_name, parent) = parent_ref(conn, dest, current.as_str()).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::caret::atspi_yields_to_ibus;

    struct PropertyText {
        offset: i32,
    }

    #[zbus::interface(name = "org.a11y.atspi.Text")]
    impl PropertyText {
        #[zbus(property)]
        fn caret_offset(&self) -> i32 {
            self.offset
        }
    }

    struct MethodText {
        offset: i32,
    }

    #[zbus::interface(name = "org.a11y.atspi.Text")]
    impl MethodText {
        async fn get_caret_offset(&self) -> i32 {
            self.offset
        }
    }

    struct PropertyAccessible {
        name: String,
    }

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl PropertyAccessible {
        #[zbus(property)]
        fn name(&self) -> String {
            self.name.clone()
        }
    }

    struct PropertyParent {
        dest: String,
        path: OwnedObjectPath,
    }

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl PropertyParent {
        #[zbus(property)]
        fn parent(&self) -> (String, OwnedObjectPath) {
            (self.dest.clone(), self.path.clone())
        }
    }

    struct MethodParent {
        dest: String,
        path: OwnedObjectPath,
    }

    #[zbus::interface(name = "org.a11y.atspi.Accessible")]
    impl MethodParent {
        async fn get_parent(&self) -> (String, OwnedObjectPath) {
            (self.dest.clone(), self.path.clone())
        }
    }

    async fn connect(address: &str) -> zbus::Result<zbus::Connection> {
        let mut last = None;
        for _ in 0..20 {
            match zbus::connection::Builder::address(address)?.build().await {
                Ok(conn) => return Ok(conn),
                Err(err) => {
                    last = Some(err);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                },
            }
        }
        Err(last.expect("dbus retries"))
    }

    #[test]
    fn x11_classified_does_not_drop_atspi_when_ibus_is_down() {
        assert!(!atspi_yields_to_ibus(Some(true), false));
    }

    #[tokio::test]
    async fn caret_offset_reads_property_when_method_is_absent() {
        let Some(bus) = crate::platform::spawn_test_bus() else {
            eprintln!("skip: dbus-daemon not available");
            return;
        };
        let _server = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("dev.emmmm.easy-complete.atspi-prop")
            .unwrap()
            .serve_at("/org/a11y/atspi/accessible/37", PropertyText { offset: 114 })
            .unwrap()
            .build()
            .await
            .expect("atspi property server");
        let client = connect(&bus.address).await.expect("client");
        let offset = caret_offset(
            &client,
            "dev.emmmm.easy-complete.atspi-prop",
            "/org/a11y/atspi/accessible/37",
        )
        .await
        .expect("CaretOffset property");
        assert_eq!(offset, 114);
    }

    #[tokio::test]
    async fn caret_offset_falls_back_to_get_caret_offset_method() {
        let Some(bus) = crate::platform::spawn_test_bus() else {
            eprintln!("skip: dbus-daemon not available");
            return;
        };
        let _server = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("dev.emmmm.easy-complete.atspi-method")
            .unwrap()
            .serve_at("/org/a11y/atspi/accessible/1", MethodText { offset: 7 })
            .unwrap()
            .build()
            .await
            .expect("atspi method server");
        let client = connect(&bus.address).await.expect("client");
        let offset = caret_offset(
            &client,
            "dev.emmmm.easy-complete.atspi-method",
            "/org/a11y/atspi/accessible/1",
        )
        .await
        .expect("GetCaretOffset method");
        assert_eq!(offset, 7);
    }

    #[tokio::test]
    async fn accessible_name_reads_property_when_get_name_is_absent() {
        let Some(bus) = crate::platform::spawn_test_bus() else {
            eprintln!("skip: dbus-daemon not available");
            return;
        };
        let _server = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("dev.emmmm.easy-complete.atspi-name")
            .unwrap()
            .serve_at(
                "/org/a11y/atspi/accessible/root",
                PropertyAccessible {
                    name: "xfce4-terminal".into(),
                },
            )
            .unwrap()
            .build()
            .await
            .expect("atspi name server");
        let client = connect(&bus.address).await.expect("client");
        let name = accessible_name(
            &client,
            "dev.emmmm.easy-complete.atspi-name",
            "/org/a11y/atspi/accessible/root",
        )
        .await
        .expect("Name property");
        assert_eq!(name, "xfce4-terminal");
    }

    #[tokio::test]
    async fn parent_reads_property_when_get_parent_is_absent() {
        let Some(bus) = crate::platform::spawn_test_bus() else {
            eprintln!("skip: dbus-daemon not available");
            return;
        };
        let parent_path: OwnedObjectPath = "/org/a11y/atspi/accessible/root".try_into().unwrap();
        let _server = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("dev.emmmm.easy-complete.atspi-parent-prop")
            .unwrap()
            .serve_at(
                "/org/a11y/atspi/accessible/child",
                PropertyParent {
                    dest: ":1.9".into(),
                    path: parent_path.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .expect("atspi parent property server");
        let client = connect(&bus.address).await.expect("client");
        let parent = parent_ref(
            &client,
            "dev.emmmm.easy-complete.atspi-parent-prop",
            "/org/a11y/atspi/accessible/child",
        )
        .await
        .expect("Parent property");
        assert_eq!(parent.0, ":1.9");
        assert_eq!(parent.1.as_str(), parent_path.as_str());
    }

    #[tokio::test]
    async fn parent_falls_back_to_get_parent_method() {
        let Some(bus) = crate::platform::spawn_test_bus() else {
            eprintln!("skip: dbus-daemon not available");
            return;
        };
        let parent_path: OwnedObjectPath = "/org/a11y/atspi/accessible/frame".try_into().unwrap();
        let _server = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("dev.emmmm.easy-complete.atspi-parent-method")
            .unwrap()
            .serve_at(
                "/org/a11y/atspi/accessible/text",
                MethodParent {
                    dest: ":1.4".into(),
                    path: parent_path.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .expect("atspi parent method server");
        let client = connect(&bus.address).await.expect("client");
        let parent = parent_ref(
            &client,
            "dev.emmmm.easy-complete.atspi-parent-method",
            "/org/a11y/atspi/accessible/text",
        )
        .await
        .expect("GetParent method");
        assert_eq!(parent.0, ":1.4");
        assert_eq!(parent.1.as_str(), parent_path.as_str());
    }
}
