use std::borrow::Cow;

use fig_proto::local::caret_position_hook::Origin;

use crate::bootstrap::WindowId;
use crate::dpi::{Position, Size};
use crate::platform::PlatformBoundEvent;

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum Event {
    WindowEvent {
        window_id: WindowId,
        window_event: WindowEvent,
    },

    PlatformBoundEvent(PlatformBoundEvent),
    /// Quit the GPUI application. Replaces the leftover tao `ControlFlow::Exit`.
    Quit,
    /// Overlay theme setting changed. The list re-reads `autocomplete.theme`.
    ThemeChanged,
    SetTrayVisible(bool),

    /// Settings file or native settings UI changed. Re-apply overlay theme
    /// and autocomplete.enabled, then recomplete if still enabled.
    ReloadSettings,
    ReloadAccessibility,
    /// Drop generateSpec / generator caches on the engine worker. The next
    /// keystroke re-runs hooks; the overlay is not forced to recomplete.
    ClearEngineCaches,
    /// Rebuild the tray icon and menu. Auth is gone, so this is never a
    /// signed-out / "session expired" state.
    ReloadTray,

    /// Menu bar or tray item activated. Delivered by `muda::MenuEvent::set_event_handler`.
    MenuClicked(String),
    /// Fresh permission check for the native settings gate.
    PermissionSnapshot(crate::permissions::PermissionSnapshot),

    /// Headless engine input for the GPUI overlay.
    GpuiOverlayBuffer {
        buffer: String,
        cwd: String,
        cursor: u32,
        session_id: uuid::Uuid,
    },
    /// The current completion request exceeded the loading threshold. This is
    /// routed through the host event queue so GPUI state is only touched from
    /// the top-level dispatcher.
    GpuiOverlayLoading {
        generation: u64,
    },
    /// The request outlived the user's script budget. Only the `···` marker is
    /// retired; the request itself keeps running under the engine watchdog.
    GpuiOverlayLoadingExpired {
        generation: u64,
    },
    /// A completion request finished on the engine thread.
    GpuiOverlayComplete {
        generation: u64,
        result: anyhow::Result<ec_engine::CompleteResult>,
        session_id: uuid::Uuid,
        cwd: String,
    },
    /// Figterm intercepted a key bound to an autocomplete action.
    AutocompleteAction {
        action: String,
        session_id: uuid::Uuid,
    },
    /// A mouse click carries the exact row that was clicked. Keeping this in
    /// the event avoids a shared pending slot being overwritten by a second
    /// click before the host event loop handles the first one.
    AutocompleteClick {
        click: ec_gpui::ClickInsert,
        session_id: uuid::Uuid,
        generation: u64,
    },

    ShowMessageNotification(ShowMessageNotification),
}

impl From<PlatformBoundEvent> for Event {
    fn from(event: PlatformBoundEvent) -> Self {
        Self::PlatformBoundEvent(event)
    }
}

impl From<ShowMessageNotification> for Event {
    fn from(event: ShowMessageNotification) -> Self {
        Self::ShowMessageNotification(event)
    }
}

#[derive(Debug, Default)]
pub struct ShowMessageNotification {
    pub title: Cow<'static, str>,
    pub body: Cow<'static, str>,
    pub parent: Option<WindowId>,
    pub buttons: Option<rfd::MessageButtons>,
    pub buttons_result: Option<tokio::sync::mpsc::Sender<rfd::MessageDialogResult>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowPosition {
    Absolute(Position),
    Centered,
    RelativeToCaret {
        caret_position: Position,
        caret_size: Size,
        origin: Origin,
    },
}

#[derive(Debug, Clone)]
pub enum WindowEvent {
    /// Sets the window to be enabled or disabled
    ///
    /// This will cause events to be ignored other than [`WindowEvent::Hide`] and
    /// [`WindowEvent::SetEnabled(true)`]
    SetEnabled(bool),
    /// Place the overlay from a caret. The WebView geometry RPC (size, anchor,
    /// measure-only) is gone; GPUI only consumes the caret position.
    UpdateWindowGeometry {
        position: Option<WindowPosition>,
    },
    /// Hides the window.
    Hide,
    /// Closes the window.
    Close,
    Show,
    NavigateRelative {
        path: Cow<'static, str>,
    },

    /// Open the native inspector equivalent. Settings maps this to Show;
    /// the overlay maps it to showing kept rows (there is no WebView).
    Devtools,
    /// `ec debug autocomplete-window`: paint the overlay window so its
    /// bounds are visible, including transparent padding.
    DebugMode(bool),

    Batch(Vec<WindowEvent>),
}

impl WindowEvent {
    pub fn is_allowed_while_disabled(&self) -> bool {
        matches!(
            self,
            WindowEvent::Hide | WindowEvent::Close | WindowEvent::SetEnabled(_)
        )
    }
}
