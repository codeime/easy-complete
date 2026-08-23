//! GPUI process host. Overlay and settings share one `Application::run`.
//! On macOS that process-wide instance is `GPUIApplication` (`NSApplication`).

use std::sync::Arc;

use ec_engine::{EngineClient, default_specs_dir};
use gpui::{App, Application, Entity};
use muda::MenuEvent;
use tao::event_loop::ControlFlow;
use tracing::{debug, error, info, trace};
use tray_icon::TrayIcon;

use crate::event::{Event, ShowMessageNotification, WindowEvent};
use crate::overlay::OverlayController;
use crate::platform::PlatformState;
use crate::tray;
#[cfg(not(target_os = "linux"))]
use crate::tray::{get_context_menu, get_icon};
use crate::webview::notification::WebviewNotificationsState;
use crate::webview::{DASHBOARD_ID, FigIdMap, WindowId};
use crate::{EventLoopProxy, EventLoopWindowTarget};
use fig_os_shim::Context;
use fig_remote_ipc::figterm::FigtermState;

pub struct DesktopHost {
    pub fig_id_map: FigIdMap,
    pub figterm_state: Arc<FigtermState>,
    pub platform_state: Arc<PlatformState>,
    pub notifications_state: Arc<WebviewNotificationsState>,
    #[allow(dead_code)]
    pub context: Arc<Context>,
    pub show_dashboard_after_normal_launch: bool,
    pub proxy: EventLoopProxy,
    pub window_target: EventLoopWindowTarget,
    pub overlay: OverlayController,
    pub settings: Option<crate::settings_ui::SettingsHandle>,
    #[allow(dead_code)] // retained for macOS/WebView host; Linux uses linux_tray task
    pub tray: Option<TrayIcon>,
}

impl DesktopHost {
    pub fn dispatch(&mut self, event: Event, cx: &mut App) {
        match event {
            Event::WindowEvent {
                window_id,
                window_event,
            } => self.dispatch_window_event(window_id, window_event, cx),
            Event::WindowEventAll { window_event } => {
                if matches!(window_event, WindowEvent::SetTheme(_)) {
                    self.overlay.apply_theme(cx);
                }
            },
            Event::ControlFlow(ControlFlow::Exit) => cx.quit(),
            Event::ControlFlow(_) => {},
            Event::ReloadTray { is_logged_in } => {
                #[cfg(target_os = "linux")]
                crate::linux_tray::reload(is_logged_in);
                #[cfg(not(target_os = "linux"))]
                if let Some(tray) = &mut self.tray {
                    tray.set_icon(Some(get_icon(is_logged_in)))
                        .map_err(|err| error!(?err))
                        .ok();
                    tray.set_icon_as_template(true);
                    tray.set_menu(Some(Box::new(get_context_menu(is_logged_in))));
                }
            },
            Event::ReloadCredentials => {
                let autocomplete_enabled = autocomplete_should_run();
                self.overlay.apply_theme(cx);
                self.overlay.set_enabled(autocomplete_enabled, cx);
                if autocomplete_enabled {
                    self.overlay.recomplete(cx);
                }
            },
            Event::ReloadAccessibility => {
                #[cfg(target_os = "linux")]
                crate::linux_tray::reload(true);
                #[cfg(not(target_os = "linux"))]
                if let Some(tray) = &mut self.tray {
                    tray.set_menu(Some(Box::new(get_context_menu(true))));
                }
                let autocomplete_enabled = autocomplete_should_run();
                self.overlay.apply_theme(cx);
                self.overlay.set_enabled(autocomplete_enabled, cx);
                self.refresh_settings_permissions();
            },
            Event::MenuClicked(id) => {
                info!(%id, "Menu Event");
                let menu_event = MenuEvent { id: muda::MenuId(id) };
                crate::webview::menu::handle_event(&menu_event, &self.proxy);
                tray::handle_event(&menu_event, &self.proxy);
            },
            Event::PermissionSnapshot(snapshot) => {
                if let Some(handle) = &self.settings {
                    crate::settings_ui::apply_permission_snapshot(handle, snapshot, cx);
                }
            },
            Event::SetTrayVisible(visible) => {
                #[cfg(target_os = "linux")]
                crate::linux_tray::set_visible(visible);
                #[cfg(not(target_os = "linux"))]
                if let Some(tray) = &mut self.tray {
                    if let Err(err) = tray.set_visible(visible) {
                        error!(%err, "Failed to set tray visible");
                    }
                }
            },
            Event::PlatformBoundEvent(native_event) => {
                if let Err(err) = self.platform_state.handle(
                    native_event,
                    &self.window_target,
                    &self.fig_id_map,
                    &self.notifications_state,
                ) {
                    debug!(%err, "Failed to handle native event");
                }
            },
            Event::ShowMessageNotification(ShowMessageNotification {
                title,
                body,
                parent,
                buttons,
                buttons_result,
            }) => {
                let dialog = rfd::AsyncMessageDialog::new().set_title(title).set_description(body);
                let _ = parent;
                let dialog = match (buttons, buttons_result.as_ref()) {
                    (Some(buttons), Some(_)) => dialog.set_buttons(buttons),
                    _ => dialog,
                };
                tokio::spawn(async move {
                    let res = dialog.show().await;
                    if let Some(buttons_result) = buttons_result {
                        buttons_result
                            .send(res)
                            .await
                            .map_err(|err| error!(?err, "Failed to send dialog result"))
                            .ok();
                    }
                });
            },
            Event::GpuiOverlayBuffer {
                buffer,
                cwd,
                cursor,
                session_id,
            } => {
                self.overlay
                    .complete_buffer(buffer, cwd, cursor, session_id, self.figterm_state.clone(), cx);
            },
            Event::GpuiOverlayLoading { generation } => {
                self.overlay.show_loading(generation, cx);
            },
            Event::GpuiOverlayLoadingExpired { generation } => {
                self.overlay.expire_loading(generation, cx);
            },
            Event::GpuiOverlayComplete {
                generation,
                result,
                session_id,
                cwd,
            } => {
                self.overlay.apply_completion(generation, result, session_id, &cwd, cx);
            },
            Event::AutocompleteAction { action, session_id } => {
                self.overlay.handle_action(&action, session_id, &self.figterm_state, cx);
            },
            Event::AutocompleteClick {
                click,
                session_id,
                generation,
            } => {
                self.overlay
                    .handle_click(click, session_id, generation, &self.figterm_state, cx);
            },
        }
    }

    fn dispatch_window_event(&mut self, window_id: WindowId, window_event: WindowEvent, cx: &mut App) {
        match window_id {
            id if id == DASHBOARD_ID => self.dispatch_settings_event(window_event, cx),
            id if id == crate::webview::AUTOCOMPLETE_ID => match window_event {
                WindowEvent::Hide | WindowEvent::Close => self.overlay.hide(cx),
                WindowEvent::SetEnabled(enabled) => self.overlay.set_enabled(enabled, cx),
                WindowEvent::UpdateWindowGeometry { position, .. } => {
                    if let Some(position) = position {
                        self.overlay.apply_position(position, &self.platform_state, cx);
                    }
                },
                WindowEvent::Show => self.overlay.show(cx),
                other => trace!(?other, "Ignoring settings-only event on GPUI overlay"),
            },
            other => trace!(%other, ?window_event, "Ignoring event for unknown window"),
        }
    }

    fn dispatch_settings_event(&mut self, window_event: WindowEvent, cx: &mut App) {
        match window_event {
            WindowEvent::Show | WindowEvent::Devtools => self.show_settings(cx),
            WindowEvent::Hide | WindowEvent::Close => self.close_settings(cx),
            WindowEvent::NavigateRelative { path } => {
                self.show_settings(cx);
                if let Some(handle) = &self.settings {
                    crate::settings_ui::set_settings_section(handle, path.as_ref(), cx);
                }
            },
            WindowEvent::Batch(events) => {
                for event in events {
                    self.dispatch_settings_event(event, cx);
                }
            },
            other => trace!(?other, "Ignoring webview-only event on native settings"),
        }
    }

    fn show_settings(&mut self, cx: &mut App) {
        if let Some(handle) = &self.settings {
            if crate::settings_ui::focus_settings(handle, cx) {
                crate::settings_ui::notify_dashboard_visible(&self.proxy, true);
                self.refresh_settings_permissions();
                return;
            }
        }
        match crate::settings_ui::open_settings_window(cx, self.proxy.clone()) {
            Ok(handle) => {
                self.settings = Some(handle);
                crate::settings_ui::notify_dashboard_visible(&self.proxy, true);
                self.refresh_settings_permissions();
            },
            Err(err) => error!(%err, "Failed to open native settings"),
        }
    }

    fn refresh_settings_permissions(&self) {
        if self.settings.is_some() {
            crate::permissions::spawn_check(&self.proxy);
        }
    }

    fn close_settings(&mut self, cx: &mut App) {
        if let Some(handle) = self.settings.take() {
            crate::settings_ui::close_settings(&handle, cx);
        }
        crate::settings_ui::notify_dashboard_visible(&self.proxy, false);
    }
}

fn autocomplete_should_run() -> bool {
    if fig_settings::settings::get_bool_or("autocomplete.disable", false) {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        macos_utils::accessibility::accessibility_is_enabled()
    }
    #[cfg(not(target_os = "macos"))]
    {
        PlatformState::accessibility_is_enabled().unwrap_or(true)
    }
}

pub fn run(host: Entity<DesktopHost>, event_rx: flume::Receiver<Event>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Ok(event) = event_rx.recv_async().await {
            let result = cx.update(|cx| {
                host.update(cx, |host, cx| host.dispatch(event, cx));
            });
            if let Err(err) = result {
                error!(%err, "GPUI app released while dispatching desktop event");
                break;
            }
        }
    })
    .detach();
}

/// GPUI subclasses `NSApplication` as `GPUIApplication` and stores ivars on that
/// instance. The first `sharedApplication` call wins, so tray-icon / settings
/// windows must not create a plain `NSApplication` first — that aborts in `set_ivar`.
#[cfg(target_os = "macos")]
pub fn ensure_gpui_ns_application() {
    #[allow(unexpected_cfgs, unused_unsafe)]
    unsafe {
        use objc::{class, msg_send, sel, sel_impl};
        let _: cocoa::base::id = msg_send![class!(GPUIApplication), sharedApplication];
    }
}

pub fn start_application(
    setup: impl FnOnce(&mut App) -> anyhow::Result<(Entity<DesktopHost>, flume::Receiver<Event>)> + 'static,
) -> anyhow::Result<()> {
    Application::new().run(move |cx: &mut App| match setup(cx) {
        Ok((host, event_rx)) => {
            run(host.clone(), event_rx, cx);
            host.update(cx, |host, _cx| {
                info!("Fig has started");
                #[cfg(target_os = "macos")]
                {
                    crate::platform::set_activation_policy(*crate::platform::ACTIVATION_POLICY.lock().unwrap());
                    let show_settings = (host.show_dashboard_after_normal_launch
                        && !crate::platform::launched_as_login_item())
                        || crate::permissions::accessibility_is_missing();
                    if show_settings {
                        host.proxy
                            .send_event(Event::WindowEvent {
                                window_id: DASHBOARD_ID,
                                window_event: WindowEvent::Show,
                            })
                            .ok();
                    }
                }
                #[cfg(not(target_os = "macos"))]
                if host.show_dashboard_after_normal_launch {
                    host.proxy
                        .send_event(Event::WindowEvent {
                            window_id: DASHBOARD_ID,
                            window_event: WindowEvent::Show,
                        })
                        .ok();
                }
            });
        },
        Err(err) => {
            error!(%err, "Failed to start GPUI host");
            cx.quit();
        },
    });
    Ok(())
}

pub fn spawn_engine() -> anyhow::Result<EngineClient> {
    let dir = default_specs_dir();
    if !dir.is_dir() {
        tracing::warn!(
            path = %dir.display(),
            "specs IR directory missing; completions fall back to cobra and history"
        );
    }
    EngineClient::spawn(dir)
}
