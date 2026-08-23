use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fig_proto::local::{EditBufferHook, InterceptedKeyHook, PostExecHook, PreExecHook, PromptHook};
use fig_proto::remote::clientbound;
use fig_remote_ipc::figterm::{FigtermState, SessionMetrics};
use time::OffsetDateTime;
use tracing::debug;
use uuid::Uuid;

use crate::event::{Event, WindowEvent};
use crate::platform::PlatformBoundEvent;
use crate::{AUTOCOMPLETE_ID, EventLoopProxy};

#[derive(Debug, Clone)]
pub struct RemoteHook {
    pub proxy: EventLoopProxy,
}

#[async_trait::async_trait]
impl fig_remote_ipc::RemoteHookHandler for RemoteHook {
    type Error = anyhow::Error;

    async fn sessions_changed(&mut self, _figterm_state: &Arc<FigtermState>) {}

    async fn edit_buffer(
        &mut self,
        hook: &EditBufferHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        if figterm_state
            .with(&session_id, |session| session.edit_buffer_hook_is_duplicate(hook))
            .unwrap_or(false)
        {
            return Ok(None);
        }
        let _old_metrics = figterm_state.with_update(session_id, |session| {
            session.edit_buffer.text.clone_from(&hook.text);
            session.edit_buffer.cursor.clone_from(&hook.cursor);
            session
                .terminal_cursor_coordinates
                .clone_from(&hook.terminal_cursor_coordinates);
            session.apply_context(hook.context.clone());

            let received_at = OffsetDateTime::now_utc();
            let current_session_expired = session
                .current_session_metrics
                .as_ref()
                .is_some_and(|metrics| received_at > metrics.end_time + Duration::from_secs(5));

            if current_session_expired {
                let previous = session.current_session_metrics.clone();
                session.current_session_metrics = Some(SessionMetrics::new(received_at));
                previous
            } else {
                if let Some(ref mut metrics) = session.current_session_metrics {
                    metrics.end_time = received_at;
                }
                None
            }
        });

        let empty_edit_buffer = hook.text.trim().is_empty();

        if !empty_edit_buffer {
            self.proxy
                .send_event(Event::PlatformBoundEvent(PlatformBoundEvent::EditBufferChanged))?;
        }

        let cwd = figterm_state
            .with(&session_id, |session| {
                session
                    .context
                    .as_ref()
                    .and_then(|ctx| ctx.current_working_directory.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        self.proxy.send_event(Event::GpuiOverlayBuffer {
            buffer: hook.text.clone(),
            cwd,
            cursor: hook.cursor.max(0) as u32,
            session_id,
        })?;

        Ok(None)
    }

    async fn prompt(
        &mut self,
        hook: &PromptHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        figterm_state.with(&session_id, |session| {
            session.apply_context(hook.context.clone());
        });

        Ok(None)
    }

    async fn pre_exec(
        &mut self,
        hook: &PreExecHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        figterm_state.with_update(session_id, |session| {
            session.apply_context(hook.context.clone());
        });

        self.proxy.send_event(Event::WindowEvent {
            window_id: AUTOCOMPLETE_ID.clone(),
            window_event: WindowEvent::Hide,
        })?;

        Ok(None)
    }

    async fn post_exec(
        &mut self,
        hook: &PostExecHook,
        session_id: Uuid,
        figterm_state: &Arc<FigtermState>,
    ) -> Result<Option<clientbound::response::Response>> {
        figterm_state.with_update(session_id, |session| {
            session.apply_context(hook.context.clone());
        });

        Ok(None)
    }

    async fn intercepted_key(
        &mut self,
        InterceptedKeyHook { action, context: _, .. }: InterceptedKeyHook,
        _session_id: Uuid,
    ) -> Result<Option<clientbound::response::Response>> {
        debug!(%action, "Intercepted Key Action");

        self.proxy.send_event(Event::AutocompleteAction {
            action,
            session_id: _session_id,
        })?;

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn send_event_and_encode_do_not_unwrap() {
        let src = include_str!("mod.rs");
        let production = src.split("#[cfg(test)]").next().expect("production");
        assert!(
            !production.contains(".unwrap()"),
            "remote_ipc must not unwrap send_event"
        );
        assert!(
            !production.contains("WindowEvent::Emit") && !production.contains("BASE64_STANDARD"),
            "WebView protobuf/base64 notification emit is gone"
        );
    }

    #[test]
    fn duplicate_edit_buffer_does_not_wake_the_overlay() {
        let src = include_str!("mod.rs");
        let start = src.find("async fn edit_buffer").expect("edit_buffer");
        let body = &src[start..];
        let end = body.find("\n    async fn prompt").expect("prompt");
        let body = &body[..end];
        assert!(
            body.contains("edit_buffer_hook_is_duplicate") && body.contains("return Ok(None)"),
            "unchanged edit-buffer hooks must not send GpuiOverlayBuffer"
        );
        let send = body.find("GpuiOverlayBuffer").expect("overlay event");
        let skip = body.find("edit_buffer_hook_is_duplicate").expect("duplicate check");
        assert!(
            skip < send,
            "duplicate check must run before cloning the buffer onto the overlay event"
        );
    }
}
