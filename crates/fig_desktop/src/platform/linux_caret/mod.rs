//! Linux caret host: X11 focus, IBus cursor geometry, and AT-SPI on GNOME Wayland.
//!
//! The overlay is driven only by `RelativeToCaret`. A focused terminal window
//! is stored so relative IBus rectangles can be converted; it is never used to
//! place the list when no caret arrives.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::Serialize;
use tao::dpi::Position;
use tracing::info;

use super::{PlatformBoundEvent, PlatformWindow};
use crate::utils::Rect;
use crate::webview::notification::WebviewNotificationsState;
use crate::webview::{FigIdMap, WindowId};
use crate::{EventLoopProxy, EventLoopWindowTarget};

mod atspi;
mod ibus;
mod x11;

static WM_RECEIVED_DATA: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone)]
pub struct PlatformWindowImpl {
    pub wm_class: String,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ActiveWindow {
    pub outer_x: i32,
    pub outer_y: i32,
    pub outer_width: i32,
    pub outer_height: i32,
    pub scale: f32,
}

#[derive(Debug, Serialize)]
pub struct PlatformStateImpl {
    #[serde(skip)]
    pub(super) proxy: EventLoopProxy,
    #[serde(skip)]
    pub(super) active_window: Mutex<Option<ActiveWindow>>,
    #[serde(skip)]
    pub(super) active_terminal: Mutex<Option<fig_util::Terminal>>,
    /// `Some(true)` focused X11 terminal, `Some(false)` other X11 client.
    /// `None` until X11 classifies a client — AT-SPI may then drive the caret.
    #[serde(skip)]
    pub(super) x11_classified: Mutex<Option<bool>>,
    #[serde(skip)]
    pub(super) atspi_focused: Mutex<Option<(String, String)>>,
}

impl PlatformStateImpl {
    pub(super) fn new(proxy: EventLoopProxy) -> Self {
        Self {
            proxy,
            active_window: Mutex::new(None),
            active_terminal: Mutex::new(None),
            x11_classified: Mutex::new(None),
            atspi_focused: Mutex::new(None),
        }
    }

    pub(super) fn handle(
        self: &Arc<Self>,
        event: PlatformBoundEvent,
        _window_target: &EventLoopWindowTarget,
        _window_map: &FigIdMap,
        _notifications_state: &Arc<WebviewNotificationsState>,
    ) -> anyhow::Result<()> {
        if matches!(event, PlatformBoundEvent::Initialize) {
            info!("linux caret host: X11 focus + IBus + AT-SPI; overlay hidden until a caret arrives");
            x11::spawn(self.proxy.clone(), Arc::clone(self));
            ibus::spawn(self.proxy.clone(), Arc::clone(self));
            atspi::spawn(self.proxy.clone(), Arc::clone(self));
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

    pub(super) fn get_active_window(&self) -> Option<PlatformWindow> {
        let window = *self.active_window.lock();
        window.map(|window| PlatformWindow {
            rect: Rect {
                position: tao::dpi::LogicalPosition::new(window.outer_x as f64, window.outer_y as f64).into(),
                size: tao::dpi::LogicalSize::new(window.outer_width as f64, window.outer_height as f64).into(),
            },
            inner: PlatformWindowImpl {
                wm_class: String::new(),
            },
        })
    }

    pub fn accessibility_is_enabled() -> Option<bool> {
        None
    }
}

pub fn autocomplete_active() -> bool {
    WM_RECEIVED_DATA.load(Ordering::Relaxed)
}

pub(super) fn mark_wm_data() {
    WM_RECEIVED_DATA.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::PlatformState;

    #[test]
    fn linux_host_has_no_accessibility_and_no_invented_caret() {
        let (proxy, _rx) = crate::event_loop::channel();
        let state = PlatformState::new(proxy);
        assert_eq!(PlatformState::accessibility_is_enabled(), None);
        assert!(state.get_cursor_position().is_none());
        assert!(state.get_active_window().is_none());
        assert!(!autocomplete_active());
    }
}
