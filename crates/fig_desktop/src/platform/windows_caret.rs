//! Windows caret via `GetGUIThreadInfo` (the Win32 caret box).
//!
//! No window-rect fallback: if `hwndCaret` is missing the overlay stays hidden.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tracing::info;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};

use super::{PlatformBoundEvent, PlatformWindow};
use crate::bootstrap::AUTOCOMPLETE_ID;
use crate::bootstrap::WindowId;
use crate::dpi::Position;
use crate::event::{Event, WindowEvent, WindowPosition};
use crate::platform::caret::{
    CaretOnScreen, Win32CaretPollAction, win32_caret_from_gui_thread, win32_caret_poll_action,
};
use crate::utils::Rect;
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
                let now = poll_caret();
                let (next_had, action) = win32_caret_poll_action(had_caret, now.is_some());
                match (action, now) {
                    (Win32CaretPollAction::Send, Some(caret)) => send_caret(&proxy, caret),
                    (Win32CaretPollAction::Hide, _) => hide(&proxy),
                    _ => {},
                }
                had_caret = next_had;
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
        let hwnd_caret_valid = !info.hwndCaret.is_invalid();
        let rect = info.rcCaret;
        let mut origin = POINT {
            x: rect.left,
            y: rect.top,
        };
        if hwnd_caret_valid {
            let _ = ClientToScreen(info.hwndCaret, &mut origin);
        }
        win32_caret_from_gui_thread(
            hwnd_caret_valid,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            origin.x,
            origin.y,
        )
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
        },
    });
}
