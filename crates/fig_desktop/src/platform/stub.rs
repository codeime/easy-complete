//! Stand-in for desktop platforms that do not yet have a caret backend
//! (currently Windows). Linux uses `linux_caret/`. Do not invent a window-rect
//! placement fallback.

use std::sync::Arc;

use serde::Serialize;
use tao::dpi::Position;
use tracing::info;

use super::{PlatformBoundEvent, PlatformWindow};
use crate::bootstrap::WindowId;
use crate::utils::Rect;
use crate::{EventLoopProxy, EventLoopWindowTarget};

#[derive(Debug)]
#[allow(dead_code)]
pub struct PlatformWindowImpl;

#[derive(Debug, Serialize)]
pub struct PlatformStateImpl {
    #[serde(skip)]
    #[allow(dead_code)]
    pub(super) proxy: EventLoopProxy,
}

impl PlatformStateImpl {
    pub(super) fn new(proxy: EventLoopProxy) -> Self {
        Self { proxy }
    }

    #[allow(clippy::unused_self)]
    pub(super) fn handle(
        self: &Arc<Self>,
        event: PlatformBoundEvent,
        _window_target: &EventLoopWindowTarget,
    ) -> anyhow::Result<()> {
        if matches!(event, PlatformBoundEvent::Initialize) {
            info!("platform stub: no caret source; overlay stays hidden");
        }
        Ok(())
    }

    #[allow(clippy::unused_self)]
    pub(super) fn position_window(&self, _window_id: &WindowId, _position: Position) -> anyhow::Result<()> {
        Ok(())
    }

    #[allow(clippy::unused_self)]
    pub(super) fn get_cursor_position(&self) -> Option<Rect> {
        None
    }

    #[allow(clippy::unused_self)]
    pub(super) fn get_active_window(&self) -> Option<PlatformWindow> {
        None
    }

    pub fn accessibility_is_enabled() -> Option<bool> {
        None
    }
}

/// No window-manager data arrives on the stub, so autocomplete is not active.
pub const fn autocomplete_active() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::PlatformState;

    #[test]
    fn stub_reports_no_accessibility_caret_or_active_window() {
        let (proxy, _rx) = crate::event_loop::channel();
        let state = PlatformState::new(proxy);
        assert_eq!(PlatformState::accessibility_is_enabled(), None);
        assert!(state.get_cursor_position().is_none());
        assert!(state.get_active_window().is_none());
        assert!(!autocomplete_active());
    }

    #[test]
    fn stub_handles_caret_events_as_no_ops() {
        let (proxy, _rx) = crate::event_loop::channel();
        let state = Arc::new(PlatformStateImpl::new(proxy));
        state
            .handle(PlatformBoundEvent::CaretPositionUpdateRequested, &EventLoopWindowTarget)
            .expect("caret request is a no-op");
        state
            .handle(PlatformBoundEvent::Initialize, &EventLoopWindowTarget)
            .expect("initialize is a no-op");
    }
}
