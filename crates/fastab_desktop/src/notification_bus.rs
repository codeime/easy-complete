use std::sync::LazyLock;

use dashmap::DashMap;
use fnv::FnvBuildHasher;
use serde_json::Value;
use tokio::sync::broadcast::{Receiver, Sender};

const CHANNEL_SIZE: usize = 8;

pub static NOTIFICATION_BUS: LazyLock<NotificationBus> = LazyLock::new(NotificationBus::new);

#[derive(Debug, Clone)]
pub enum JsonNotification {
    Created { value: Value },
    Changed { value: Value },
    Removed,
}

impl JsonNotification {
    pub fn value(self) -> Option<Value> {
        match self {
            JsonNotification::Created { value } | JsonNotification::Changed { value } => Some(value),
            // A deleted key has no value, so subscribers fall back to the setting's default.
            JsonNotification::Removed => None,
        }
    }

    pub fn into_bool(self) -> Option<bool> {
        self.value().and_then(|value| value.as_bool())
    }

    pub fn into_string(self) -> Option<String> {
        self.value().and_then(|value| value.as_str().map(|s| s.into()))
    }
}

#[derive(Debug, Default)]
pub struct NotificationBus {
    settings_channels: DashMap<String, Sender<JsonNotification>, FnvBuildHasher>,
}

impl NotificationBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe_settings(&self, key: String) -> Receiver<JsonNotification> {
        self.settings_channels
            .entry(key)
            .or_insert_with(|| {
                let (tx, _) = tokio::sync::broadcast::channel(CHANNEL_SIZE);
                tx
            })
            .subscribe()
    }

    pub fn send_settings_new(&self, key: impl AsRef<str>, value: &Value) {
        if let Some(tx) = self.settings_channels.get(key.as_ref()) {
            tx.send(JsonNotification::Created { value: value.clone() }).ok();
        }
    }

    pub fn send_settings_remove(&self, key: impl AsRef<str>) {
        if let Some(tx) = self.settings_channels.get(key.as_ref()) {
            tx.send(JsonNotification::Removed).ok();
        }
    }

    pub fn send_settings_changed(&self, key: impl AsRef<str>, new: &Value) {
        if let Some(tx) = self.settings_channels.get(key.as_ref()) {
            tx.send(JsonNotification::Changed { value: new.clone() }).ok();
        }
    }
}
