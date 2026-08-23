//! Listen for IBus `SetCursorLocation` / `SetCursorLocationRelative` and emit
//! `RelativeToCaret`.
//!
//! GTK IM calls those methods on the IBus private bus. A full session bus has
//! `org.freedesktop.DBus.Monitoring.BecomeMonitor`; IBus 1.5.32's private bus
//! does not. `dbus-monitor` falls back to `AddMatch` with `eavesdrop='true'`.
//! Do the same: BecomeMonitor when it exists, otherwise eavesdrop AddMatch.
//! zbus `MatchRule` has no eavesdrop field, so the fallback is a raw AddMatch.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use fig_util::terminal::PositioningKind;
use tracing::{debug, info, warn};
use zbus::fdo::MonitoringProxy;
use zbus::message::Type as MessageType;
use zbus::{MatchRule, MessageStream};

use super::PlatformStateImpl;
use crate::EventLoopProxy;
use crate::event::{Event, WindowEvent, WindowPosition};
use crate::platform::caret::{caret_from_ibus_absolute, caret_from_ibus_relative};
use crate::webview::AUTOCOMPLETE_ID;

pub(crate) const IBUS_INPUT_CONTEXT_IFACE: &str = "org.freedesktop.IBus.InputContext";

/// dbus-monitor fallback when the bus has no Monitoring interface.
pub(crate) const IBUS_EAVESDROP_MATCH_RULE: &str =
    "type='method_call',interface='org.freedesktop.IBus.InputContext',eavesdrop='true'";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IbusSubscribeKind {
    BecomeMonitor,
    EavesdropAddMatch,
}

impl IbusSubscribeKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BecomeMonitor => "BecomeMonitor",
            Self::EavesdropAddMatch => "eavesdrop AddMatch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IbusCursorMethod {
    Absolute,
    Relative,
}

pub(crate) fn ibus_cursor_method(interface: Option<&str>, member: Option<&str>) -> Option<IbusCursorMethod> {
    if interface != Some(IBUS_INPUT_CONTEXT_IFACE) {
        return None;
    }
    match member {
        Some("SetCursorLocation") => Some(IbusCursorMethod::Absolute),
        Some("SetCursorLocationRelative") => Some(IbusCursorMethod::Relative),
        _ => None,
    }
}

/// True when BecomeMonitor failed because the bus has no Monitoring interface.
pub(crate) fn monitoring_interface_missing(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::MethodError(name, _, _) => {
            let name = name.as_str();
            name == "org.freedesktop.DBus.Error.UnknownMethod" || name == "org.freedesktop.DBus.Error.UnknownInterface"
        },
        zbus::Error::FDO(fdo) => monitoring_fdo_missing(fdo),
        zbus::Error::InterfaceNotFound => true,
        other => {
            let text = other.to_string();
            text.contains("UnknownMethod") || text.contains("UnknownInterface") || text.contains("Monitoring")
        },
    }
}

fn monitoring_fdo_missing(err: &zbus::fdo::Error) -> bool {
    matches!(
        err,
        zbus::fdo::Error::UnknownMethod(_) | zbus::fdo::Error::UnknownInterface(_)
    ) || err.to_string().contains("Monitoring")
}

struct IbusListeningGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for IbusListeningGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub fn spawn(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) {
    tokio::spawn(async move {
        loop {
            match listen(proxy.clone(), state.clone()).await {
                Ok(()) => debug!("IBus caret listener ended"),
                Err(err) => warn!(%err, "IBus caret listener failed"),
            }
            state.ibus_listening.store(false, Ordering::Release);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn listen(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) -> anyhow::Result<()> {
    let conn = zbus::connection::Builder::ibus()?.build().await?;
    let kind = subscribe_set_cursor_location(&conn).await?;
    info!(kind = kind.as_str(), "IBus caret subscribed");
    state.ibus_listening.store(true, Ordering::Release);
    let _listening = IbusListeningGuard(&state.ibus_listening);
    let mut stream = MessageStream::from(conn);
    use futures::StreamExt;
    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                warn!(%err, "IBus message stream error");
                break;
            },
        };
        if msg.message_type() != MessageType::MethodCall {
            continue;
        }
        let header = msg.header();
        let Some(kind) = ibus_cursor_method(
            header.interface().map(|i| i.as_str()),
            header.member().map(|m| m.as_str()),
        ) else {
            continue;
        };
        match kind {
            IbusCursorMethod::Absolute => {
                if state.x11_classified.lock().as_ref() != Some(&true) {
                    continue;
                }
                if state.active_terminal.lock().is_none() {
                    continue;
                }
                let Ok((x, y, w, h)) = msg.body().deserialize::<(i32, i32, i32, i32)>() else {
                    continue;
                };
                let kind = state
                    .active_terminal
                    .lock()
                    .as_ref()
                    .map_or(PositioningKind::Physical, |terminal| terminal.positioning_kind());
                let Some(caret) = caret_from_ibus_absolute(x, y, w, h, kind) else {
                    continue;
                };
                send_caret(&proxy, caret);
            },
            IbusCursorMethod::Relative => {
                if state.x11_classified.lock().as_ref() != Some(&true) {
                    continue;
                }
                let Some(window) = *state.active_window.lock() else {
                    continue;
                };
                let Ok((x, y, w, h)) = msg.body().deserialize::<(i32, i32, i32, i32)>() else {
                    continue;
                };
                let Some(caret) = caret_from_ibus_relative(x, y, w, h, window.outer_x, window.outer_y, window.scale)
                else {
                    continue;
                };
                send_caret(&proxy, caret);
            },
        }
    }
    Ok(())
}

pub(crate) async fn subscribe_set_cursor_location(conn: &zbus::Connection) -> anyhow::Result<IbusSubscribeKind> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::MethodCall)
        .interface(IBUS_INPUT_CONTEXT_IFACE)?
        .build();
    match MonitoringProxy::new(conn).await {
        Ok(monitor) => match monitor.become_monitor(std::slice::from_ref(&rule), 0).await {
            Ok(()) => return Ok(IbusSubscribeKind::BecomeMonitor),
            Err(err) if monitoring_fdo_missing(&err) => {
                debug!(%err, "IBus bus has no Monitoring; eavesdrop AddMatch");
            },
            Err(err) => return Err(err.into()),
        },
        Err(err) if monitoring_interface_missing(&err) => {
            debug!(%err, "IBus Monitoring proxy failed; eavesdrop AddMatch");
        },
        Err(err) => return Err(err.into()),
    }
    add_eavesdrop_match(conn).await?;
    Ok(IbusSubscribeKind::EavesdropAddMatch)
}

async fn add_eavesdrop_match(conn: &zbus::Connection) -> zbus::Result<()> {
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "AddMatch",
        &IBUS_EAVESDROP_MATCH_RULE,
    )
    .await?;
    Ok(())
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
    use zbus::fdo;

    #[test]
    fn eavesdrop_match_rule_is_the_dbus_monitor_fallback() {
        assert!(IBUS_EAVESDROP_MATCH_RULE.contains("eavesdrop='true'"));
        assert!(IBUS_EAVESDROP_MATCH_RULE.contains("type='method_call'"));
        assert!(IBUS_EAVESDROP_MATCH_RULE.contains(IBUS_INPUT_CONTEXT_IFACE));
        assert!(
            !IBUS_EAVESDROP_MATCH_RULE.contains("BecomeMonitor"),
            "eavesdrop AddMatch must not require Monitoring"
        );
        assert!(
            !IBUS_EAVESDROP_MATCH_RULE.contains("destination="),
            "eavesdrop must see other clients' SetCursorLocation, not only our own"
        );
    }

    #[test]
    fn monitoring_unknown_method_is_a_fallback_not_a_hard_failure() {
        let err = zbus::Error::FDO(Box::new(fdo::Error::UnknownMethod(
            "No such interface “org.freedesktop.DBus.Monitoring” on object at path /org/freedesktop/DBus".into(),
        )));
        assert!(monitoring_interface_missing(&err));

        let err = zbus::Error::FDO(Box::new(fdo::Error::UnknownInterface(
            "org.freedesktop.DBus.Monitoring".into(),
        )));
        assert!(monitoring_interface_missing(&err));

        let msg = zbus::message::Message::method_call("/", "BecomeMonitor")
            .unwrap()
            .build(&())
            .unwrap();
        let name: zbus::names::OwnedErrorName = "org.freedesktop.DBus.Error.UnknownMethod".try_into().unwrap();
        let err = zbus::Error::MethodError(
            name,
            Some("No such interface “org.freedesktop.DBus.Monitoring”".into()),
            msg,
        );
        assert!(monitoring_interface_missing(&err));

        let err = zbus::Error::FDO(Box::new(fdo::Error::AccessDenied("no".into())));
        assert!(!monitoring_interface_missing(&err));
    }

    #[test]
    fn set_cursor_location_body_is_screen_xywh() {
        let msg = zbus::message::Message::method_call("/org/freedesktop/IBus/InputContext_1", "SetCursorLocation")
            .unwrap()
            .interface(IBUS_INPUT_CONTEXT_IFACE)
            .unwrap()
            .build(&(186i32, 230i32, 10i32, 19i32))
            .unwrap();
        assert_eq!(
            ibus_cursor_method(
                msg.header().interface().map(|i| i.as_str()),
                msg.header().member().map(|m| m.as_str()),
            ),
            Some(IbusCursorMethod::Absolute)
        );
        let (x, y, w, h): (i32, i32, i32, i32) = msg.body().deserialize().unwrap();
        assert_eq!((x, y, w, h), (186, 230, 10, 19));
        assert!(crate::platform::caret::ibus_rect_is_usable(x, y, w, h));
    }

    #[test]
    fn other_input_context_members_are_not_a_caret() {
        assert_eq!(
            ibus_cursor_method(Some(IBUS_INPUT_CONTEXT_IFACE), Some("FocusIn")),
            None
        );
        assert_eq!(
            ibus_cursor_method(Some("org.freedesktop.IBus.Panel"), Some("SetCursorLocation")),
            None
        );
        assert_eq!(
            ibus_cursor_method(Some(IBUS_INPUT_CONTEXT_IFACE), Some("SetCursorLocationRelative")),
            Some(IbusCursorMethod::Relative)
        );
    }

    #[tokio::test]
    async fn subscribe_sees_foreign_set_cursor_location_without_ibus_daemon() {
        let Some(bus) = crate::platform::spawn_test_bus() else {
            eprintln!("skip: dbus-daemon not available");
            return;
        };
        let listener = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .await
            .expect("listener");
        let kind = tokio::time::timeout(Duration::from_secs(2), subscribe_set_cursor_location(&listener))
            .await
            .expect("subscribe timed out")
            .expect("subscribe");
        assert!(
            kind == IbusSubscribeKind::BecomeMonitor || kind == IbusSubscribeKind::EavesdropAddMatch,
            "unexpected subscribe kind {kind:?}"
        );

        let _service = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("dev.emmmm.easy-complete.ibus-caret-test")
            .unwrap()
            .build()
            .await
            .expect("service");

        let mut stream = MessageStream::from(&listener);
        let client = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .await
            .expect("client");
        let outgoing = zbus::message::Message::method_call("/org/freedesktop/IBus/InputContext_1", "SetCursorLocation")
            .unwrap()
            .destination("dev.emmmm.easy-complete.ibus-caret-test")
            .unwrap()
            .interface(IBUS_INPUT_CONTEXT_IFACE)
            .unwrap()
            .with_flags(zbus::message::Flags::NoReplyExpected)
            .unwrap()
            .build(&(186i32, 230i32, 10i32, 19i32))
            .unwrap();
        client.send(&outgoing).await.expect("send SetCursorLocation");

        use futures::StreamExt;
        let seen = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(msg) = stream.next().await {
                let Ok(msg) = msg else {
                    continue;
                };
                if msg.message_type() != MessageType::MethodCall {
                    continue;
                }
                let header = msg.header();
                if ibus_cursor_method(
                    header.interface().map(|i| i.as_str()),
                    header.member().map(|m| m.as_str()),
                ) == Some(IbusCursorMethod::Absolute)
                {
                    let body: (i32, i32, i32, i32) = msg.body().deserialize().ok()?;
                    return Some(body);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert_eq!(
            seen,
            Some((186, 230, 10, 19)),
            "subscribe kind was {kind:?}; foreign SetCursorLocation must be visible"
        );
    }
}
