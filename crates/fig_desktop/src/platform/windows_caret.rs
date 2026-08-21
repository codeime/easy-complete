//! Windows caret via `GetGUIThreadInfo` (the Win32 caret box).
//!
//! No window-rect fallback: if `hwndCaret` is missing the overlay stays hidden.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tao::dpi::{PhysicalPosition, PhysicalSize, Position};
use tracing::info;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};

use super::{PlatformBoundEvent, PlatformWindow};
use crate::event::{Event, WindowEvent, WindowPosition};
use crate::platform::caret::{CaretOnScreen, ibus_rect_is_usable};
use crate::utils::Rect;
use crate::webview::AUTOCOMPLETE_ID;
use crate::webview::notification::WebviewNotificationsState;
use crate::webview::{FigIdMap, WindowId};
use crate::{EventLoopProxy, EventLoopWindowTarget};

static SAW_CARET: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
#[allow(dead_code)]
pub struct PlatformWindowImpl;

#[derive(Debug, Serialize)]
pub struct PlatformStateImpl {
    #[serde(skip)]
    pub(super) proxy: EventLoopProxy,
}

impl PlatformStateImpl {
    pub(super) fn new(proxy: EventLoopProxy) -> Self {
        Self { proxy }
    }

    pub(super) fn handle(
        self: &Arc<Self>,
        event: PlatformBoundEvent,
        _window_target: &EventLoopWindowTarget,
        _window_map: &FigIdMap,
        _notifications_state: &Arc<WebviewNotificationsState>,
    ) -> anyhow::Result<()> {
        if matches!(event, PlatformBoundEvent::Initialize) {
            info!("windows caret host: GetGUIThreadInfo; overlay hidden until a caret arrives");
            spawn(self.proxy.clone());
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

pub fn autocomplete_active() -> bool {
    SAW_CARET.load(Ordering::Relaxed)
}

fn spawn(proxy: EventLoopProxy) {
    thread::Builder::new()
        .name("ec-win-caret".into())
        .spawn(move || {
            let mut had_caret = false;
            loop {
                match poll_caret() {
                    Some(caret) => {
                        had_caret = true;
                        send_caret(&proxy, caret);
                    },
                    None => {
                        if had_caret {
                            hide(&proxy);
                            had_caret = false;
                        }
                    },
                }
                thread::sleep(Duration::from_millis(16));
            }
        })
        .ok();
}

fn poll_caret() -> Option<CaretOnScreen> {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground.is_invalid() {
            return None;
        }
        let tid = GetWindowThreadProcessId(foreground, None);
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(tid, &mut info).ok()?;
        if info.hwndCaret.is_invalid() {
            return None;
        }
        let rect = info.rcCaret;
        let mut origin = POINT {
            x: rect.left,
            y: rect.top,
        };
        let _ = ClientToScreen(info.hwndCaret, &mut origin);
        let width = (rect.right - rect.left).max(1);
        let height = rect.bottom - rect.top;
        if !ibus_rect_is_usable(origin.x, origin.y, width, height) {
            return None;
        }
        Some(CaretOnScreen {
            position: PhysicalPosition::new(origin.x, origin.y).into(),
            size: PhysicalSize::new(width, height).into(),
            origin: fig_proto::local::caret_position_hook::Origin::TopLeft,
        })
    }
}

fn hide(proxy: &EventLoopProxy) {
    let _ = proxy.send_event(Event::WindowEvent {
        window_id: AUTOCOMPLETE_ID,
        window_event: WindowEvent::Hide,
    });
}

fn send_caret(proxy: &EventLoopProxy, caret: CaretOnScreen) {
    SAW_CARET.store(true, Ordering::Relaxed);
    let _ = proxy.send_event(Event::WindowEvent {
        window_id: AUTOCOMPLETE_ID,
        window_event: WindowEvent::UpdateWindowGeometry {
            position: Some(WindowPosition::RelativeToCaret {
                caret_position: caret.position,
                caret_size: caret.size,
                origin: caret.origin,
            }),
            size: None,
            anchor: None,
            tx: None,
            dry_run: false,
        },
    });
}
