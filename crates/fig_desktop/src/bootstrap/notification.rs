//! Protobuf notification subscriptions for native windows.
//!
//! GPUI overlay does not subscribe. Kept for dump-state and leftover IPC
//! listeners. Do not wire a WebView back through this.

use anyhow::Result;
use base64::prelude::*;
use bytes::BytesMut;
use dashmap::DashMap;
use fig_proto::fig::server_originated_message::Submessage as ServerOriginatedSubMessage;
use fig_proto::fig::{Notification, NotificationType, ServerOriginatedMessage};
use fig_proto::prost::Message;
use fnv::FnvBuildHasher;
use tracing::debug;

use super::WindowId;
use crate::EventLoopProxy;
use crate::event::{EmitEventName, Event, WindowEvent};

#[derive(Debug, Default)]
pub struct NotificationWindowState(pub DashMap<NotificationType, i64, FnvBuildHasher>);

impl std::ops::Deref for NotificationWindowState {
    type Target = DashMap<NotificationType, i64, FnvBuildHasher>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Default)]
pub struct NotificationsState {
    pub subscriptions: DashMap<WindowId, NotificationWindowState, FnvBuildHasher>,
}

impl NotificationsState {
    pub async fn broadcast_notification_all(
        &self,
        notification_type: &NotificationType,
        notification: Notification,
        proxy: &EventLoopProxy,
    ) -> Result<()> {
        if self.subscriptions.is_empty() {
            return Ok(());
        }

        debug!(?notification_type, "Broadcasting window notification");

        for sub in self.subscriptions.iter() {
            let message_id = match sub.get(notification_type) {
                Some(id) => *id,
                None => continue,
            };

            let message = ServerOriginatedMessage {
                id: Some(message_id),
                submessage: Some(ServerOriginatedSubMessage::Notification(notification.clone())),
            };

            let mut encoded = BytesMut::new();
            message.encode(&mut encoded)?;

            proxy.send_event(Event::WindowEvent {
                window_id: sub.key().clone(),
                window_event: WindowEvent::Emit {
                    event_name: EmitEventName::Notification,
                    payload: BASE64_STANDARD.encode(encoded).into(),
                },
            })?;
        }

        Ok(())
    }
}

impl serde::Serialize for NotificationWindowState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for subscription in self.0.iter() {
            map.serialize_entry(subscription.key(), &subscription.value())?;
        }
        map.end()
    }
}

impl serde::Serialize for NotificationsState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(Some(self.subscriptions.len()))?;
        for subscription in self.subscriptions.iter() {
            map.serialize_entry(subscription.key(), &subscription.value())?;
        }
        map.end()
    }
}
