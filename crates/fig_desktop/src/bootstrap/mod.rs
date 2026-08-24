//! Desktop process bootstrap: event loop, tray, IPC, then GPUI.
//!
//! This directory used to host WKWebView. Overlay and settings are GPUI views
//! now. Do not put wry / WKWebView back.

pub mod menu;
pub mod window_id;

use std::sync::{Arc, OnceLock};

use fig_os_shim::Context;
use fig_remote_ipc::figterm::FigtermState;
use gpui::AppContext as _;
use tracing::{debug, error, warn};

use self::menu::menu_bar;
pub use self::window_id::{AUTOCOMPLETE_ID, SETTINGS_ID, WindowId};
use crate::event::Event;
use crate::notification_bus::{JsonNotification, NOTIFICATION_BUS};
use crate::platform::{PlatformBoundEvent, PlatformState};
use crate::remote_ipc::RemoteHook;
#[cfg(not(target_os = "linux"))]
use crate::tray::build_tray;
use crate::{EventLoopProxy, EventLoopWindowTarget, file_watcher, local_ipc};

pub const AUTOCOMPLETE_WINDOW_TITLE: &str = ec_gpui::OVERLAY_WINDOW_TITLE;

pub struct AppRuntime {
    pub(crate) event_rx: flume::Receiver<Event>,
    pub(crate) proxy: EventLoopProxy,
    pub(crate) figterm_state: Arc<FigtermState>,
    pub(crate) platform_state: Arc<PlatformState>,
    pub(crate) context: Arc<Context>,
    pub(crate) show_settings_after_normal_launch: bool,
}

/// The platform layer reaches for this from callbacks that carry no state of
/// their own. Everything else is threaded through [`AppRuntime`].
pub static GLOBAL_PROXY: OnceLock<EventLoopProxy> = OnceLock::new();

fn diagnostic_exit(message: &str) -> ! {
    error!("{message}");
    eprintln!("easy-complete: {message}");
    #[allow(clippy::exit)]
    std::process::exit(1);
}

impl AppRuntime {
    #[allow(unused_variables)]
    #[allow(unused_mut)]
    pub fn new(context: Arc<Context>, visible: bool, show_settings_after_normal_launch: bool) -> Self {
        #[cfg(target_os = "macos")]
        if !visible {
            use crate::platform::ACTIVATION_POLICY;
            use crate::platform::caret::MacosActivationPolicy;

            *crate::utils::recover_mutex(&ACTIVATION_POLICY) = MacosActivationPolicy::Accessory;
            crate::platform::set_activation_policy(MacosActivationPolicy::Accessory);
        }

        let (proxy, event_rx) = crate::event_loop::channel();
        if GLOBAL_PROXY.set(proxy.clone()).is_err() {
            diagnostic_exit("event loop proxy already initialized");
        }

        let figterm_state = Arc::new(FigtermState::default());
        let platform_state = Arc::new(PlatformState::new(proxy.clone()));

        Self {
            event_rx,
            proxy,
            figterm_state,
            platform_state,
            context,
            show_settings_after_normal_launch,
        }
    }

    #[allow(unused_mut)]
    pub async fn run(mut self) -> anyhow::Result<()> {
        let window_target = EventLoopWindowTarget;
        if let Err(err) = self
            .platform_state
            .handle(PlatformBoundEvent::Initialize, &window_target)
        {
            diagnostic_exit(&format!("failed to initialize platform state: {err:#}"));
        }

        {
            let platform_state = self.platform_state.clone();
            let figterm_state = self.figterm_state.clone();
            let proxy = self.proxy.clone();
            tokio::spawn(async move {
                match local_ipc::start_local_ipc(platform_state, figterm_state, proxy).await {
                    Ok(_) => (),
                    Err(err) => error!("Unable to start local ipc: {:?}", err),
                }
            });
        }

        match fig_util::directories::local_remote_socket_path() {
            Ok(path) => {
                tokio::spawn(fig_remote_ipc::remote::start_remote_ipc(
                    path,
                    self.figterm_state.clone(),
                    RemoteHook {
                        proxy: self.proxy.clone(),
                    },
                ));
            },
            Err(err) => error!(%err, "Unable to start remote ipc"),
        }

        file_watcher::setup_listeners(self.proxy.clone()).await;

        init_notification_listeners(self.proxy.clone()).await;

        let menu_proxy = self.proxy.clone();
        muda::MenuEvent::set_event_handler(Some({
            let menu_proxy = menu_proxy.clone();
            move |event: muda::MenuEvent| {
                let _ = menu_proxy.send_event(Event::MenuClicked(event.id.0));
            }
        }));
        // Handler install is OnceCell: if anything primed it to `None` first, new events stay
        // on the muda channel. Drain leftovers and keep a fallback listener so tray clicks
        // cannot go silent again.
        while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            let _ = menu_proxy.send_event(Event::MenuClicked(event.id.0));
        }
        std::thread::Builder::new()
            .name("ec-menu-events".into())
            .spawn({
                let menu_proxy = menu_proxy.clone();
                move || {
                    let rx = muda::MenuEvent::receiver();
                    while let Ok(event) = rx.recv() {
                        let _ = menu_proxy.send_event(Event::MenuClicked(event.id.0));
                    }
                }
            })
            .ok();

        let tray_visible = !fig_settings::settings::get_bool_or("app.hideMenubarIcon", false);
        #[cfg(target_os = "linux")]
        {
            crate::linux_tray::spawn();
            crate::linux_tray::set_visible(tray_visible);
        }
        #[cfg(not(target_os = "linux"))]
        let tray = match build_tray(&window_target, &self.figterm_state).await {
            Ok(tray) => {
                if let Err(err) = tray.set_visible(tray_visible) {
                    error!(%err, "Failed to set tray visible");
                }
                Some(tray)
            },
            Err(err) => {
                error!(%err, "Failed to create tray icon; continuing without a tray");
                None
            },
        };
        #[cfg(target_os = "linux")]
        let tray = None;

        #[allow(unused_variables)]
        let menu_bar = menu_bar();
        #[cfg(target_os = "macos")]
        menu_bar.init_for_nsapp();

        if let Err(err) = self
            .proxy
            .send_event(Event::PlatformBoundEvent(PlatformBoundEvent::InitializePostRun))
        {
            diagnostic_exit(&format!("failed to send post-init event: {err}"));
        }

        let engine = match crate::gpui_host::spawn_engine() {
            Ok(engine) => engine,
            Err(err) => diagnostic_exit(&format!("failed to start completion engine: {err:#}")),
        };
        let event_rx = self.event_rx;
        let proxy = self.proxy.clone();
        let figterm_state = self.figterm_state;
        let platform_state = self.platform_state;
        let context = self.context;
        let show_settings_after_normal_launch = self.show_settings_after_normal_launch;

        if let Err(err) = crate::gpui_host::start_application(move |cx| {
            let overlay = crate::overlay::OverlayController::start(
                cx,
                engine,
                proxy.clone(),
                figterm_state.clone(),
                platform_state.clone(),
            )?;
            let host = cx.new(|_| crate::gpui_host::DesktopHost {
                figterm_state,
                platform_state,
                context,
                show_settings_after_normal_launch,
                proxy,
                window_target,
                overlay,
                settings: None,
                tray,
            });
            Ok((host, event_rx))
        }) {
            diagnostic_exit(&format!("failed to start gpui host: {err:#}"));
        }
        Ok(())
    }
}

async fn init_notification_listeners(proxy: EventLoopProxy) {
    #[allow(unused_macros)]
    macro_rules! watcher {
        ($type:ident, $name:expr, $on_update:expr) => {{
            paste::paste! {
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    let mut rx = NOTIFICATION_BUS.[<subscribe_ $type>]($name.into());
                    loop {
                        let res = rx.recv().await;
                        match res {
                            Ok(val) => {
                                #[allow(clippy::redundant_closure_call)]
                                ($on_update)(val, &proxy);
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Notification bus '{}' lagged by {n} messages", $name);
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });
            }
        };};
    }

    #[cfg(target_os = "linux")]
    {
        use fig_integrations::desktop_entry::should_install_autostart_entry;
        use fig_settings::{Settings, State};

        use crate::event::WindowEvent;
        use crate::notification_bus::JsonNotification;
        watcher!(
            settings,
            "autocomplete.disable",
            |notification: JsonNotification, proxy: &EventLoopProxy| {
                let enabled = !notification.into_bool().unwrap_or(false);
                debug!(%enabled, "Autocomplete");
                proxy.send_event_or_warn(Event::WindowEvent {
                    window_id: AUTOCOMPLETE_ID.clone(),
                    window_event: WindowEvent::SetEnabled(enabled),
                });
            }
        );
        watcher!(
            settings,
            "app.launchOnStartup",
            |notification: JsonNotification, _proxy: &EventLoopProxy| {
                let enabled = notification.into_bool().unwrap_or(false);
                debug!(%enabled, "app.launchOnStartup");
                tokio::spawn(async move {
                    let ctx = Context::new();
                    let settings = Settings::new();
                    let state = State::new();
                    let enabled = should_install_autostart_entry(&ctx, &settings, &state);
                    if let Err(err) = fig_integrations::launch_at_login::set_enabled_in(&ctx, enabled).await {
                        warn!(?err, "unable to update autostart integration");
                    }
                });
            }
        );
    }

    watcher!(
        settings,
        "app.theme",
        |notification: JsonNotification, proxy: &EventLoopProxy| {
            debug!(theme = ?notification.into_string(), "Theme changed");
            proxy.send_event_or_warn(Event::ThemeChanged);
        }
    );

    watcher!(
        settings,
        "app.hideMenubarIcon",
        |notification: JsonNotification, proxy: &EventLoopProxy| {
            let enabled = !notification.into_bool().unwrap_or(false);
            debug!(%enabled, "Tray icon");
            proxy.send_event_or_warn(Event::SetTrayVisible(enabled));
        }
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn bootstrap_startup_does_not_unwrap() {
        let production = include_str!("mod.rs").split("#[cfg(test)]").next().expect("production");
        assert!(
            !production.contains("local_remote_socket_path().unwrap()"),
            "a missing remote socket path must skip remote IPC, not panic the desktop"
        );
        assert!(
            !production.contains("Failed to initialize platform state"),
            "platform Initialize failure must diagnostic_exit, not panic"
        );
        assert!(
            !production.contains(".expect(\"gpui host\")"),
            "GPUI host failure must diagnostic_exit, not panic"
        );
        assert!(
            production.contains("local_remote_socket_path()")
                && production.contains("failed to initialize platform state")
                && production.contains("failed to start gpui host"),
            "startup still initializes platform, remote IPC, and GPUI"
        );
    }
}
