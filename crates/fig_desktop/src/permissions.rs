//! Native permission checks used by the settings gate (replaces the dashboard WebView gate).

use std::sync::Arc;

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
use crate::install_request::install;

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
        permission_snapshot_all_ready(
            self.shell,
            self.accessibility,
            self.input_method,
            crate::platform::caret::permission_gate_requires_ax_and_ime(cfg!(target_os = "macos")),
        )
    }

    pub fn still_checking(&self) -> bool {
        permission_snapshot_still_checking(
            self.shell,
            self.accessibility,
            self.input_method,
            crate::platform::caret::permission_gate_requires_ax_and_ime(cfg!(target_os = "macos")),
        )
    }
}

fn permission_snapshot_all_ready(
    shell: PermReady,
    accessibility: PermReady,
    input_method: PermReady,
    require_ax_and_ime: bool,
) -> bool {
    shell == PermReady::Ready
        && (!require_ax_and_ime || (accessibility == PermReady::Ready && input_method == PermReady::Ready))
}

fn permission_snapshot_still_checking(
    shell: PermReady,
    accessibility: PermReady,
    input_method: PermReady,
    require_ax_and_ime: bool,
) -> bool {
    matches!(shell, PermReady::Checking)
        || (require_ax_and_ime
            && (matches!(accessibility, PermReady::Checking) || matches!(input_method, PermReady::Checking)))
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
                let not_supported: i32 = InstallationStatus::NotSupported.into();
                Ok(status == installed || status == not_supported)
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
    let (shell, shell_err) = ready_from(query(PermId::Shell, InstallAction::Status).await);
    #[cfg(target_os = "macos")]
    {
        let (ax, ax_err) = ready_from(query(PermId::Accessibility, InstallAction::Status).await);
        let (ime, ime_err) = ready_from(query(PermId::InputMethod, InstallAction::Status).await);
        let error = ax_err.or(shell_err).or(ime_err);
        PermissionSnapshot {
            accessibility: ax,
            shell,
            input_method: ime,
            error,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionSnapshot {
            accessibility: PermReady::Ready,
            shell,
            input_method: PermReady::Ready,
            error: shell_err,
        }
    }
}

pub async fn repair(id: PermId) -> Result<(), String> {
    let _ = query(id, InstallAction::Install).await?;
    Ok(())
}

fn permission_repair_ids(require_ax_and_ime: bool) -> &'static [PermId] {
    if require_ax_and_ime {
        &[PermId::Accessibility, PermId::Shell, PermId::InputMethod]
    } else {
        &[PermId::Shell]
    }
}

fn repair_ids() -> &'static [PermId] {
    permission_repair_ids(crate::platform::caret::permission_gate_requires_ax_and_ime(cfg!(
        target_os = "macos"
    )))
}

pub async fn repair_all() -> Result<(), String> {
    for id in repair_ids() {
        if let Err(err) = repair(*id).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ready_matches_the_platform_gate() {
        let ax_and_ime_broken = PermissionSnapshot {
            accessibility: PermReady::Missing,
            shell: PermReady::Ready,
            input_method: PermReady::Error,
            error: None,
        };
        let shell_missing = PermissionSnapshot {
            accessibility: PermReady::Ready,
            shell: PermReady::Missing,
            input_method: PermReady::Ready,
            error: None,
        };
        let ax_checking = PermissionSnapshot {
            accessibility: PermReady::Checking,
            shell: PermReady::Ready,
            input_method: PermReady::Checking,
            error: None,
        };
        assert!(!permission_snapshot_all_ready(
            PermReady::Ready,
            PermReady::Missing,
            PermReady::Error,
            true
        ));
        assert!(permission_snapshot_all_ready(
            PermReady::Ready,
            PermReady::Missing,
            PermReady::Error,
            false
        ));
        assert!(!permission_snapshot_all_ready(
            PermReady::Missing,
            PermReady::Ready,
            PermReady::Ready,
            false
        ));
        assert!(permission_snapshot_all_ready(
            PermReady::Ready,
            PermReady::Checking,
            PermReady::Checking,
            false
        ));
        assert!(!permission_snapshot_still_checking(
            PermReady::Ready,
            PermReady::Checking,
            PermReady::Checking,
            false
        ));
        assert!(permission_snapshot_still_checking(
            PermReady::Ready,
            PermReady::Checking,
            PermReady::Checking,
            true
        ));
        assert_eq!(
            permission_repair_ids(true),
            &[PermId::Accessibility, PermId::Shell, PermId::InputMethod]
        );
        assert_eq!(permission_repair_ids(false), &[PermId::Shell]);
        #[cfg(target_os = "macos")]
        {
            assert!(!ax_and_ime_broken.all_ready());
            assert!(!shell_missing.all_ready());
            assert!(ax_checking.still_checking());
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(
                ax_and_ime_broken.all_ready(),
                "Linux/Windows settings must not wait on Accessibility/IME"
            );
            assert!(!shell_missing.all_ready());
            assert!(!ax_and_ime_broken.still_checking());
            assert!(ax_checking.all_ready());
        }
        let src = include_str!("permissions.rs");
        assert!(
            src.contains("permission_gate_requires_ax_and_ime(cfg!(target_os = \"macos\"))"),
            "the settings gate must take the shared macOS AX+IME flag, not a second cfg table"
        );
        let host = include_str!("gpui_host.rs");
        assert!(
            host.contains("autocomplete_may_run"),
            "ReloadSettings enable must use the shared disable/AX gate"
        );
    }
}
