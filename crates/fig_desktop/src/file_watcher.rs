use fig_settings::JsonStore;
use fig_util::directories;
use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::{Map, Value};
use tracing::{debug, error, trace, warn};

use crate::Event;
use crate::EventLoopProxy;
use crate::notification_bus::NOTIFICATION_BUS;

pub async fn setup_listeners(proxy: EventLoopProxy) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let mut watcher = match notify::recommended_watcher(move |res| match res {
        Ok(event) => {
            if let Err(err) = tx.send(event) {
                error!(%err, "failed to send notify event");
            }
        },
        Err(err) => error!(%err, "notify watcher"),
    }) {
        Ok(watcher) => watcher,
        Err(err) => {
            warn!(%err, "failed to create settings file watcher; settings live-reload disabled");
            return;
        },
    };

    let settings_path = match directories::settings_path() {
        Ok(settings_path) => match settings_path.parent() {
            Some(settings_dir) => match watcher.watch(settings_dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    trace!("watching settings file at {settings_dir:?}");
                    Some(settings_path)
                },
                Err(err) => {
                    error!(%err, "failed to watch settings dir");
                    None
                },
            },
            None => {
                error!("failed to get settings file dir");
                None
            },
        },
        Err(err) => {
            error!(%err, "failed to get settings file path");
            None
        },
    };

    tokio::spawn(async move {
        let _watcher = watcher;

        let mut prev_settings = match fig_settings::OldSettings::load_from_file() {
            Ok(map) => map,
            Err(err) => {
                error!(?err, "failed to initialize settings");
                Map::new()
            },
        };

        #[cfg(target_os = "linux")]
        {
            use crate::Event;
            use crate::bootstrap::AUTOCOMPLETE_ID;
            use crate::event::WindowEvent;
            proxy
                .send_event(Event::WindowEvent {
                    window_id: AUTOCOMPLETE_ID,
                    window_event: WindowEvent::SetEnabled(
                        !prev_settings
                            .get("autocomplete.disable")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    ),
                })
                .map_err(|err| error!(?err, "failed initializing autocomplete.disable state"))
                .ok();
        }

        while let Some(event) = rx.recv().await {
            trace!(?event, "Settings event");

            if let Some(settings_path) = &settings_path {
                if event.paths.contains(settings_path) {
                    if let EventKind::Create(_) | EventKind::Modify(_) = event.kind {
                        match fig_settings::OldSettings::load_from_file() {
                            Ok(settings) => {
                                debug!("Settings file changed");

                                if let Err(err) = fig_settings::settings::init_global() {
                                    error!(%err, "failed to reload settings into memory");
                                }
                                proxy
                                    .send_event(Event::ReloadCredentials)
                                    .map_err(|err| error!(?err, "failed to refresh overlay after settings change"))
                                    .ok();

                                json_map_diff(
                                    &prev_settings,
                                    &settings,
                                    |key, value| {
                                        debug!(%key, %value, "Setting added");
                                        NOTIFICATION_BUS.send_settings_new(key, value);
                                    },
                                    |key, old, new| {
                                        debug!(%key, %old, %new, "Setting change");
                                        NOTIFICATION_BUS.send_settings_changed(key, new);
                                    },
                                    |key, value| {
                                        debug!(%key, %value, "Setting removed");
                                        NOTIFICATION_BUS.send_settings_remove(key);
                                    },
                                );

                                prev_settings = settings;
                            },
                            Err(err) => error!(%err, "Failed to get settings"),
                        }
                    }
                }
            }
        }
    });
}

// Diffs the old and new settings and calls the appropriate callbacks
fn json_map_diff(
    map_a: &Map<String, Value>,
    map_b: &Map<String, Value>,
    on_new: impl Fn(&str, &Value),
    on_changed: impl Fn(&str, &Value, &Value),
    on_removed: impl Fn(&str, &Value),
) {
    for (key, value) in map_a {
        if let Some(other_value) = map_b.get(key) {
            if value != other_value {
                on_changed(key, value, other_value);
            }
        } else {
            on_removed(key, value);
        }
    }

    for (key, value) in map_b {
        if !map_a.contains_key(key) {
            on_new(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn settings_watcher_creation_does_not_panic() {
        let src = include_str!("file_watcher.rs");
        let production = src.split("#[cfg(test)]").next().expect("production");
        // Concat so this pin's own source does not contain the old panic.
        assert!(
            !production.contains(&[")\n    .unwrap", "();"].concat()),
            "recommended_watcher must warn and return instead of panicking the desktop"
        );
        assert!(
            src.contains("failed to create settings file watcher"),
            "a bad watch setup should warn and disable live-reload"
        );
    }
}
