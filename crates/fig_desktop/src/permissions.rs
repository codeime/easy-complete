//! Native permission checks used by the settings gate (replaces the dashboard WebView gate).

use std::sync::Arc;

use fig_desktop_api::requests::install::install;
use fig_os_shim::{Context, ContextArcProvider, ContextProvider};
use fig_proto::fig::install_response::{InstallationStatus, Response};
use fig_proto::fig::result::Result as ProtoResultEnum;
use fig_proto::fig::server_originated_message::Submessage as ServerOriginatedSubMessage;
use fig_proto::fig::{InstallAction, InstallComponent, InstallRequest};
use fig_settings::State;
use fig_settings::settings::{Settings, SettingsProvider};
use fig_settings::state::StateProvider;
use tracing::warn;

use crate::EventLoopProxy;
use crate::event::Event;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermId {
    Accessibility,
    Shell,
    InputMethod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermReady {
    Checking,
    Ready,
    Missing,
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct PermissionSnapshot {
    pub accessibility: PermReady,
    pub shell: PermReady,
    pub input_method: PermReady,
    pub error: Option<String>,
}

impl Default for PermReady {
    fn default() -> Self {
        Self::Checking
    }
}

impl PermissionSnapshot {
    pub fn checking() -> Self {
        Self {
            accessibility: PermReady::Checking,
            shell: PermReady::Checking,
            input_method: PermReady::Checking,
            error: None,
        }
    }

    pub fn all_ready(&self) -> bool {
        self.accessibility == PermReady::Ready
            && self.shell == PermReady::Ready
            && self.input_method == PermReady::Ready
    }

    pub fn still_checking(&self) -> bool {
        matches!(self.accessibility, PermReady::Checking)
            || matches!(self.shell, PermReady::Checking)
            || matches!(self.input_method, PermReady::Checking)
    }
}

struct InstallCtx {
    settings: Settings,
    state: State,
    ctx: Arc<Context>,
}

impl SettingsProvider for InstallCtx {
    fn settings(&self) -> &Settings {
        &self.settings
    }
}

impl StateProvider for InstallCtx {
    fn state(&self) -> &State {
        &self.state
    }
}

impl ContextProvider for InstallCtx {
    fn context(&self) -> &Context {
        self.ctx.as_ref()
    }
}

impl ContextArcProvider for InstallCtx {
    fn context_arc(&self) -> Arc<Context> {
        Arc::clone(&self.ctx)
    }
}

fn install_ctx() -> InstallCtx {
    InstallCtx {
        settings: Settings::new(),
        state: State::new(),
        ctx: Context::new(),
    }
}

fn component(id: PermId) -> InstallComponent {
    match id {
        PermId::Accessibility => InstallComponent::Accessibility,
        PermId::Shell => InstallComponent::Dotfiles,
        PermId::InputMethod => InstallComponent::InputMethod,
    }
}

fn status_from_message(msg: ServerOriginatedSubMessage) -> Result<bool, String> {
    match msg {
        ServerOriginatedSubMessage::InstallResponse(response) => match response.response {
            Some(Response::InstallationStatus(status)) => {
                let installed: i32 = InstallationStatus::Installed.into();
                Ok(status == installed)
            },
            Some(Response::Result(result)) => {
                let ok: i32 = ProtoResultEnum::Ok.into();
                if result.result == ok {
                    Ok(true)
                } else {
                    Err(result.error.unwrap_or_else(|| "Install failed".into()))
                }
            },
            None => Err("Empty install response".into()),
        },
        ServerOriginatedSubMessage::Error(err) => Err(err),
        other => Err(format!("Unexpected install response: {other:?}")),
    }
}

async fn query(id: PermId, action: InstallAction) -> Result<bool, String> {
    let ctx = install_ctx();
    let request = InstallRequest {
        component: component(id).into(),
        action: action.into(),
    };
    match install(request, &ctx).await {
        Ok(msg) => status_from_message(*msg),
        Err(err) => Err(err.to_string()),
    }
}

fn ready_from(result: Result<bool, String>) -> (PermReady, Option<String>) {
    match result {
        Ok(true) => (PermReady::Ready, None),
        Ok(false) => (PermReady::Missing, None),
        Err(err) => (PermReady::Error, Some(err)),
    }
}

pub async fn check_all() -> PermissionSnapshot {
    let (ax, ax_err) = ready_from(query(PermId::Accessibility, InstallAction::Status).await);
    let (shell, shell_err) = ready_from(query(PermId::Shell, InstallAction::Status).await);
    let (ime, ime_err) = ready_from(query(PermId::InputMethod, InstallAction::Status).await);
    let error = ax_err.or(shell_err).or(ime_err);
    PermissionSnapshot {
        accessibility: ax,
        shell,
        input_method: ime,
        error,
    }
}

pub async fn repair(id: PermId) -> Result<(), String> {
    let _ = query(id, InstallAction::Install).await?;
    Ok(())
}

pub async fn repair_all() -> Result<(), String> {
    for id in [PermId::Accessibility, PermId::Shell, PermId::InputMethod] {
        if let Err(err) = repair(id).await {
            warn!(?id, %err, "permission repair failed");
            return Err(err);
        }
    }
    Ok(())
}

pub fn spawn_check(proxy: &EventLoopProxy) {
    let proxy = proxy.clone();
    tokio::spawn(async move {
        let snapshot = check_all().await;
        if proxy.send_event(Event::PermissionSnapshot(snapshot)).is_err() {
            warn!("failed to deliver permission snapshot");
        }
    });
}

pub fn spawn_repair(proxy: &EventLoopProxy, id: PermId) {
    let proxy = proxy.clone();
    tokio::spawn(async move {
        if let Err(err) = repair(id).await {
            warn!(?id, %err, "permission repair failed");
        }
        let snapshot = check_all().await;
        proxy.send_event(Event::PermissionSnapshot(snapshot)).ok();
        proxy.send_event(Event::ReloadAccessibility).ok();
    });
}

pub fn spawn_repair_all(proxy: &EventLoopProxy) {
    let proxy = proxy.clone();
    tokio::spawn(async move {
        if let Err(err) = repair_all().await {
            warn!(%err, "permission repair-all failed");
        }
        let snapshot = check_all().await;
        proxy.send_event(Event::PermissionSnapshot(snapshot)).ok();
        proxy.send_event(Event::ReloadAccessibility).ok();
    });
}

pub fn accessibility_is_missing() -> bool {
    #[cfg(target_os = "macos")]
    {
        !macos_utils::accessibility::accessibility_is_enabled()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}
