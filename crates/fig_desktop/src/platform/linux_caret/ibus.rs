//! Listen for IBus `SetCursorLocation` / `SetCursorLocationRelative` and emit
//! `RelativeToCaret`. Connection uses zbus's IBus address helper.

use std::sync::Arc;
use std::time::Duration;

use fig_util::terminal::PositioningKind;
use tracing::{debug, warn};
use zbus::fdo::MonitoringProxy;
use zbus::message::Type as MessageType;
use zbus::{MatchRule, MessageStream};

use super::PlatformStateImpl;
use crate::EventLoopProxy;
use crate::event::{Event, WindowEvent, WindowPosition};
use crate::platform::caret::{caret_from_ibus_absolute, caret_from_ibus_relative};
use crate::webview::AUTOCOMPLETE_ID;

pub fn spawn(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) {
    tokio::spawn(async move {
        loop {
            match listen(proxy.clone(), state.clone()).await {
                Ok(()) => debug!("IBus caret listener ended"),
                Err(err) => warn!(%err, "IBus caret listener failed"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn listen(proxy: EventLoopProxy, state: Arc<PlatformStateImpl>) -> anyhow::Result<()> {
    let conn = zbus::connection::Builder::ibus()?.build().await?;
    let rule = MatchRule::builder()
        .interface("org.freedesktop.IBus.InputContext")?
        .build();
    // Foreign SetCursorLocation calls are only visible as a bus monitor.
    // AddMatch without eavesdrop delivers only this connection's unique name.
    let monitor = MonitoringProxy::new(&conn).await?;
    monitor.become_monitor(&[rule], 0).await?;
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
        let Some(interface) = header.interface() else {
            continue;
        };
        if interface.as_str() != "org.freedesktop.IBus.InputContext" {
            continue;
        }
        let Some(member) = header.member() else {
            continue;
        };
        match member.as_str() {
            "SetCursorLocation" => {
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
            "SetCursorLocationRelative" => {
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
            _ => {},
        }
    }
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
