//! Event-loop types used by `fig_desktop` after the process host moved from tao to GPUI.
//!
//! Background tasks (IPC, accessibility, tray) still post [`crate::event::Event`]s. The GPUI
//! application drains them on the AppKit main thread. This replaces `tao::event_loop::EventLoopProxy`
//! so the rest of the crate can keep calling `send_event`.

use std::fmt;

use tao::platform::macos::ActivationPolicy;

use crate::event::Event;

/// Error returned when the GPUI host has shut down and can no longer accept events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventLoopClosed;

impl fmt::Display for EventLoopClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("event loop is closed")
    }
}

impl std::error::Error for EventLoopClosed {}

/// Cloneable sender used by IPC, tray, and platform observers.
#[derive(Clone, Debug)]
pub struct EventLoopProxy {
    tx: flume::Sender<Event>,
}

impl EventLoopProxy {
    pub(crate) fn new(tx: flume::Sender<Event>) -> Self {
        Self { tx }
    }

    /// Post an event to the GPUI host.
    pub fn send_event(&self, event: Event) -> Result<(), EventLoopClosed> {
        self.tx.send(event).map_err(|_err| EventLoopClosed)
    }
}

/// Stand-in for tao's window-target. Dashboard windows are created with AppKit directly; this
/// type only carries activation-policy changes onto `NSApplication`.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventLoopWindowTarget;

impl EventLoopWindowTarget {
    /// Apply an `NSApplicationActivationPolicy` at runtime.
    #[cfg(target_os = "macos")]
    pub fn set_activation_policy_at_runtime(&self, policy: ActivationPolicy) {
        crate::platform::set_activation_policy(policy);
    }

    #[cfg(not(target_os = "macos"))]
    pub fn set_activation_policy_at_runtime(&self, _policy: ActivationPolicy) {}
}

pub(crate) fn channel() -> (EventLoopProxy, flume::Receiver<Event>) {
    let (tx, rx) = flume::unbounded();
    (EventLoopProxy::new(tx), rx)
}
