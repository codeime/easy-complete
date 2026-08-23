use anyhow::Result;
use fig_proto::local::{CaretPositionHook, FileChangedHook, FocusedWindowDataHook};
use tao::dpi::{LogicalPosition, LogicalSize};

use crate::event::{WindowEvent, WindowPosition};
use crate::platform::PlatformState;
use crate::{AUTOCOMPLETE_ID, Event, EventLoopProxy};

/// The overlay is positioned relative to the caret, so a degenerate rect is not a small error —
/// an all-zero rect resolves to the screen-space origin and gets clamped to the corner of the
/// primary monitor, which on a multi-monitor setup is a different screen than the terminal.
/// Input method clients that no longer back a live window report exactly that, so drop it here
/// rather than move the window somewhere the user is not looking.
fn is_valid_caret_rect(x: f64, y: f64, width: f64, height: f64) -> bool {
    x.is_finite() && y.is_finite() && width.is_finite() && height.is_finite() && height > 0.0
}

pub async fn caret_position(
    hook @ CaretPositionHook {
        x, y, width, height, ..
    }: CaretPositionHook,
    proxy: &EventLoopProxy,
) -> Result<()> {
    if !is_valid_caret_rect(x, y, width, height) {
        tracing::warn!(x, y, width, height, "ignoring invalid caret position hook");
        return Ok(());
    }

    proxy.send_event_or_warn(Event::WindowEvent {
        window_id: AUTOCOMPLETE_ID,
        window_event: WindowEvent::UpdateWindowGeometry {
            position: Some(WindowPosition::RelativeToCaret {
                caret_position: LogicalPosition::new(x, y).into(),
                caret_size: LogicalSize::new(width, height).into(),
                origin: hook.origin(),
            }),
            size: None,
            anchor: None,
            tx: None,
            dry_run: false,
        },
    });

    Ok(())
}

pub async fn focus_change(proxy: &EventLoopProxy) -> Result<()> {
    proxy.send_event_or_warn(Event::WindowEvent {
        window_id: AUTOCOMPLETE_ID.clone(),
        window_event: WindowEvent::Hide,
    });

    Ok(())
}

pub async fn file_changed(_file_changed_hook: FileChangedHook) -> Result<()> {
    Ok(())
}

#[allow(clippy::unused_async)]
pub async fn focused_window_data(
    hook: FocusedWindowDataHook,
    platform_state: &PlatformState,
    proxy: &EventLoopProxy,
) -> Result<()> {
    // Window-rect geometry is not a caret. Linux places only from
    // RelativeToCaret (IBus/X11). Keep this hook a no-op.
    let (_hook, _platform_state, _proxy) = (hook, platform_state, proxy);
    Ok(())
}

pub async fn event() -> Result<()> {
    // WebView JS event bus is gone. Native overlay does not subscribe.
    Ok(())
}

pub async fn clear_autocomplete_cache(proxy: &EventLoopProxy) -> Result<()> {
    proxy.send_event_or_warn(Event::ClearEngineCaches);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zeroed_caret_rect() {
        assert!(!is_valid_caret_rect(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn rejects_caret_rect_without_height() {
        assert!(!is_valid_caret_rect(1200.0, 800.0, 8.0, 0.0));
        assert!(!is_valid_caret_rect(1200.0, 800.0, 8.0, -16.0));
    }

    #[test]
    fn rejects_non_finite_caret_rect() {
        assert!(!is_valid_caret_rect(f64::NAN, 800.0, 8.0, 16.0));
        assert!(!is_valid_caret_rect(1200.0, f64::INFINITY, 8.0, 16.0));
    }

    #[test]
    fn accepts_caret_rect_on_a_secondary_monitor() {
        assert!(is_valid_caret_rect(-1920.0, -450.0, 0.0, 16.0));
        assert!(is_valid_caret_rect(1200.0, 800.0, 8.0, 16.0));
    }

    #[test]
    fn clear_autocomplete_cache_reaches_the_engine_worker() {
        let production = include_str!("hooks.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            production.contains("Event::ClearEngineCaches"),
            "ClearAutocompleteCache must drop engine generateSpec / generator caches"
        );
        assert!(
            !production.contains("Engine caches are request-keyed"),
            "the hook is no longer a WebView-era no-op"
        );
    }
}
