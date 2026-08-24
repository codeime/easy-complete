//! Event-loop types used by `fig_desktop` after the process host moved from wry/tao to GPUI.
//!
//! Background tasks (IPC, accessibility, tray) still post [`crate::event::Event`]s. The GPUI
//! application drains them on its UI thread. This replaces the old windowing crate's
//! event-loop proxy so the rest of the crate can keep calling `send_event`.

use std::fmt;

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

    /// Post an event, logging if the GPUI host has already shut down.
    pub fn send_event_or_warn(&self, event: Event) {
        if let Err(err) = self.send_event(event) {
            tracing::warn!(%err, "dropped event; event loop is closed");
        }
    }
}

/// Stand-in for the old window-target, threaded through every
/// `PlatformState::handle` so the backends keep one signature. Settings windows
/// are GPUI, so no backend carries state on it any more.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventLoopWindowTarget;

pub(crate) fn channel() -> (EventLoopProxy, flume::Receiver<Event>) {
    let (tx, rx) = flume::unbounded();
    (EventLoopProxy::new(tx), rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    #[test]
    fn send_event_or_warn_does_not_panic_after_disconnect() {
        let (proxy, rx) = channel();
        drop(rx);
        proxy.send_event_or_warn(Event::Quit);
        assert!(proxy.send_event(Event::Quit).is_err());
    }
}
