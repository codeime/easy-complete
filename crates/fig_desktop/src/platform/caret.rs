//! Shared caret conversion and overlay policy that must stay OS-agnostic.
//!
//! A terminal window rectangle is only used to turn *relative* IBus
//! coordinates into a caret. It is never a placement fallback: if there is
//! no usable caret, the overlay stays hidden.
//!
//! Linux/Windows call the IBus/Win32 helpers; macOS calls
//! [`hide_overlay_on_element_change`]. The module is compiled on every OS so
//! Linux CI pins conversion and IME-vs-AX policy without AppKit.

#![allow(dead_code)]

use fig_proto::local::caret_position_hook::Origin;
use fig_util::Terminal;
use fig_util::terminal::PositioningKind;

use crate::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Size};

/// IME-only terminals (Otty, Ghostty, Kitty, …) report the caret through IMK,
/// not AX, so an in-window focused-element change is noise we cannot follow
/// rather than a pane switch we should park the list for. No built-in terminal
/// is both IME and xterm; that guard is for a custom terminal declaring both,
/// where the AX pane switch is the real signal.
///
/// String policy, no AppKit. `macos.rs` calls this; Linux CI pins it.
pub(crate) fn hide_overlay_on_element_change(bundle_id: &str) -> bool {
    !matches!(
        Terminal::from_bundle_id(bundle_id),
        Some(terminal) if terminal.supports_macos_input_method() && !terminal.is_xterm()
    )
}

/// NSApplication activation policy, without AppKit.
///
/// `macos.rs` is `cfg(macos)`. Linux CI pins the NS integer mapping and the
/// fullscreen / settings-window rule here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MacosActivationPolicy {
    Regular,
    Accessory,
    Prohibited,
}

/// `NSApplicationActivationPolicyRegular` is 0, Accessory 1, Prohibited 2.
pub(crate) fn macos_ns_activation_policy(policy: MacosActivationPolicy) -> i64 {
    match policy {
        MacosActivationPolicy::Regular => 0,
        MacosActivationPolicy::Accessory => 1,
        MacosActivationPolicy::Prohibited => 2,
    }
}

/// Fullscreen always hides the dock icon (Accessory). Otherwise Regular only
/// while the settings window is up.
pub(crate) fn macos_settings_activation_policy(fullscreen: bool, settings_visible: bool) -> MacosActivationPolicy {
    if fullscreen || !settings_visible {
        MacosActivationPolicy::Accessory
    } else {
        MacosActivationPolicy::Regular
    }
}

/// A short AX timeout so one hung target app cannot freeze the desktop.
pub(crate) const MACOS_AX_MESSAGING_TIMEOUT_SECS: f32 = 0.25;

/// `CGWindowLevel` 0 is the normal window level. Only those windows become
/// the AX follow target. A non-zero level (iTerm Quake, always-on-top) still
/// updates overlay stacking via [`ec_gpui::macos_overlay_level_for_terminal`].
pub(crate) fn macos_stores_focused_window_at_level(level: Option<i64>) -> bool {
    level == Some(0)
}

/// Inputs for [`macos_overlay_enabled_for_focus`]. Packed so the AppKit host
/// cannot drift a sixth `bool` past clippy, and Linux tests can name each
/// flag.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MacosOverlayEnable {
    pub is_known_terminal: bool,
    pub integration_disabled: bool,
    pub supports_ime: bool,
    pub ime_enabled: bool,
    pub autocomplete_disabled: bool,
    pub accessibility_enabled: bool,
}

/// Whether the overlay may follow this focused terminal.
///
/// Unknown bundle IDs are off. IME terminals need the helper enabled; AX
/// terminals do not. A per-process "needs restart" stamp used to gate this
/// too and silently disabled autocomplete in every terminal that was already
/// open when the IME was installed — do not put that back. Not live AX/IME.
pub(crate) fn macos_overlay_enabled_for_focus(input: MacosOverlayEnable) -> bool {
    input.is_known_terminal
        && !input.integration_disabled
        && (!input.supports_ime || input.ime_enabled)
        && !input.autocomplete_disabled
        && input.accessibility_enabled
}

/// Cached `enabled` can lag a live TCC grant. The next keystroke re-reads AX
/// instead of waiting for the notification. `macos.rs` / `overlay.rs` call
/// this; Linux CI pins it.
pub(crate) fn macos_overlay_enable_from_live_ax(
    live_ax: bool,
    overlay_env_enabled: bool,
    autocomplete_disabled: bool,
) -> bool {
    live_ax && overlay_env_enabled && !autocomplete_disabled
}

/// Autocomplete is off when the setting is set. macOS also needs AX; Linux
/// and Windows pass `accessibility_granted = true` because they have no TCC.
pub(crate) fn autocomplete_may_run(disabled: bool, accessibility_granted: bool) -> bool {
    !disabled && accessibility_granted
}

/// Settings gate: only macOS waits on Accessibility + IME. Linux/Windows
/// complete from PTY + the edit buffer.
pub(crate) fn permission_gate_requires_ax_and_ime(os_is_macos: bool) -> bool {
    os_is_macos
}

/// Screen-space caret the overlay already consumes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretOnScreen {
    pub position: Position,
    pub size: Size,
    pub origin: Origin,
}

/// IBus emits `(0, 0, 0, 0)` when it has no cursor. Height ≤ 0 is the same
/// class of unusable rect the IPC caret hook already rejects.
pub fn ibus_rect_is_usable(x: i32, y: i32, width: i32, height: i32) -> bool {
    let _ = (x, y, width);
    height > 0
}

/// `SetCursorLocation` is already in screen space. Konsole reports logical
/// pixels; other Linux terminals report physical.
pub fn caret_from_ibus_absolute(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    kind: PositioningKind,
) -> Option<CaretOnScreen> {
    if !ibus_rect_is_usable(x, y, width, height) {
        return None;
    }
    let (position, size) = match kind {
        PositioningKind::Logical => (
            LogicalPosition::new(x as f64, y as f64).into(),
            LogicalSize::new(width as f64, height as f64).into(),
        ),
        PositioningKind::Physical => (
            PhysicalPosition::new(x, y).into(),
            PhysicalSize::new(width, height).into(),
        ),
    };
    Some(CaretOnScreen {
        position,
        size,
        origin: Origin::TopLeft,
    })
}

/// `SetCursorLocationRelative` is relative to the focused window's outer
/// top-left. IBus `y` is the bottom of the caret box, so height is subtracted
/// to get the overlay's top-left origin.
pub fn caret_from_ibus_relative(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    window_outer_x: i32,
    window_outer_y: i32,
    scale: f32,
) -> Option<CaretOnScreen> {
    if !ibus_rect_is_usable(x, y, width, height) {
        return None;
    }
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let abs_x = (x as f32 / scale).round() as i32 + window_outer_x;
    let abs_y = (y as f32 / scale).round() as i32 + window_outer_y - (height as f32 / scale).round() as i32;
    let abs_w = (width as f32 / scale).round() as i32;
    let abs_h = (height as f32 / scale).round() as i32;
    if abs_h <= 0 {
        return None;
    }
    Some(CaretOnScreen {
        position: LogicalPosition::new(abs_x as f64, abs_y as f64).into(),
        size: LogicalSize::new(abs_w as f64, abs_h as f64).into(),
        origin: Origin::TopLeft,
    })
}

/// AT-SPI `GetCharacterExtents(..., SCREEN)` is already a top-left screen rect.
/// Window `GetExtents` must not go through here — that is a window box, not a caret.
pub fn caret_from_atspi_extents(x: i32, y: i32, width: i32, height: i32) -> Option<CaretOnScreen> {
    caret_from_ibus_absolute(x, y, width, height, PositioningKind::Physical)
}

/// Win32 `GUITHREADINFO.rcCaret` after `ClientToScreen` of the top-left.
///
/// `hwndCaret` missing is a caller concern (return `None` before this). A
/// zero-height box is not a caret; a zero-width box is still a caret and is
/// widened to 1px so the overlay has a place to sit. Compiled on every OS so
/// Windows CI geometry is pinned without a Windows host.
pub fn caret_from_win32_client_caret(
    client_left: i32,
    client_top: i32,
    client_right: i32,
    client_bottom: i32,
    screen_x: i32,
    screen_y: i32,
) -> Option<CaretOnScreen> {
    let width = (client_right - client_left).max(1);
    let height = client_bottom - client_top;
    if !ibus_rect_is_usable(screen_x, screen_y, width, height) {
        return None;
    }
    Some(CaretOnScreen {
        position: PhysicalPosition::new(screen_x, screen_y).into(),
        size: PhysicalSize::new(width, height).into(),
        origin: Origin::TopLeft,
    })
}

/// `GetGUIThreadInfo` + `ClientToScreen` result. A missing `hwndCaret` is not
/// a window-rect fallback — the overlay parks. Compiled on every OS so Linux
/// CI pins that contract; live GetGUIThreadInfo still needs a Windows host.
pub fn win32_caret_from_gui_thread(
    hwnd_caret_valid: bool,
    client_left: i32,
    client_top: i32,
    client_right: i32,
    client_bottom: i32,
    screen_x: i32,
    screen_y: i32,
) -> Option<CaretOnScreen> {
    if !hwnd_caret_valid {
        return None;
    }
    caret_from_win32_client_caret(client_left, client_top, client_right, client_bottom, screen_x, screen_y)
}

/// What the 16 ms Win32 caret poll does with a miss after a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Win32CaretPollAction {
    Send,
    Hide,
    Idle,
}

pub fn win32_caret_poll_action(had_caret: bool, saw_caret: bool) -> (bool, Win32CaretPollAction) {
    if saw_caret {
        (true, Win32CaretPollAction::Send)
    } else if had_caret {
        (false, Win32CaretPollAction::Hide)
    } else {
        (false, Win32CaretPollAction::Idle)
    }
}

/// `AtspiRole` from at-spi2-core. Frame/Window are the toplevels we may
/// query for an IBus-relative origin; Application is the walk stop.
pub const ATSPI_ROLE_FRAME: u32 = 23;
pub const ATSPI_ROLE_WINDOW: u32 = 69;
pub const ATSPI_ROLE_APPLICATION: u32 = 75;

pub fn atspi_state_changed_is_focus_gained(kind: &str, detail1: i32) -> bool {
    kind == "focused" && detail1 != 0
}

pub fn atspi_is_self_app(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    lower == fig_util::consts::APP_PROCESS_NAME
        || lower == fig_util::consts::linux::DESKTOP_APP_WM_CLASS
        || lower == fig_util::PRODUCT_NAME.to_ascii_lowercase()
        || lower.contains("easy-complete")
}

/// D2: X11 terminals prefer IBus. AT-SPI is the GNOME Wayland caret.
///
/// Yield only while IBus is actually subscribed. A failed IBus subscribe
/// (no `BecomeMonitor`, eavesdrop AddMatch also failing) must not stand
/// AT-SPI down, or a real `GetCharacterExtents` box is thrown away.
pub fn atspi_yields_to_ibus(x11_classified: Option<bool>, ibus_listening: bool) -> bool {
    x11_classified == Some(true) && ibus_listening
}

/// at-spi2 2.56 dropped these as methods; they are D-Bus properties.
pub const ATSPI_IFACE_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
pub const ATSPI_IFACE_TEXT: &str = "org.a11y.atspi.Text";
pub const ATSPI_PROP_NAME: &str = "Name";
pub const ATSPI_PROP_CARET_OFFSET: &str = "CaretOffset";
pub const ATSPI_PROP_PARENT: &str = "Parent";
pub const ATSPI_METHOD_GET_NAME: &str = "GetName";
pub const ATSPI_METHOD_GET_CARET_OFFSET: &str = "GetCaretOffset";
pub const ATSPI_METHOD_GET_PARENT: &str = "GetParent";
/// atspi `GetCharacterExtents` / `GetExtents` coord type: screen, not window.
pub const ATSPI_COORD_TYPE_SCREEN: u32 = 0;
pub const ATSPI_COORD_TYPE_WINDOW: u32 = 1;

/// Bottom-left caret origins need a primary-screen height to convert into the
/// overlay's top-left space. Top-left (IBus / X11) does not.
pub fn caret_origin_needs_screens(origin: Origin) -> bool {
    matches!(origin, Origin::BottomLeft)
}

/// Empty `overlay_screens` on Linux/Windows means there is no display geometry
/// to place against, so the list parks. macOS Quartz TopLeft is already global
/// (AX), so a missing `NSScreen` list is not a placement fallback — it just
/// skips monitor clamping. BottomLeft (IME) still needs the primary height to
/// flip; without it the Y is not a caret.
pub(crate) fn overlay_parks_caret_when_screens_empty(os_is_macos: bool, origin: Origin) -> bool {
    if os_is_macos {
        caret_origin_needs_screens(origin)
    } else {
        true
    }
}

/// AX `kAXBoundsForRange` width is ignored for placement. A 0-width insertion
/// point still needs a box, so the overlay sits on a fixed 10px caret.
pub(crate) const MACOS_AX_DEFAULT_CARET_WIDTH: f64 = 10.0;

/// A selected range longer than one character is copy/paste, not a caret
/// (ENG-109). A 0×0 bounds box is what AX returns when the caret is gone —
/// placing the list there flashes it in the bottom corner. `macos.rs` applies
/// this; `caret_position.rs` only reports whether the AX calls succeeded.
/// Not live AX.
pub(crate) fn macos_ax_caret_is_usable(selected_range_length: i64, width: f64, height: f64) -> bool {
    selected_range_length <= 1 && !(width == 0.0 && height == 0.0)
}

/// Desktop posts this so the IME helper re-queries the caret for IME-only
/// terminals (Otty / Ghostty / Kitty). The name is a leftover Amazon Q
/// identifier: both processes must keep it in lockstep. Not live IMK.
pub(crate) const MACOS_IME_CARET_REQUEST_NOTIFICATION: &str = "com.amazon.codewhisperer.edit_buffer_updated";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_activation_policy_maps_to_ns_integers() {
        assert_eq!(macos_ns_activation_policy(MacosActivationPolicy::Regular), 0);
        assert_eq!(macos_ns_activation_policy(MacosActivationPolicy::Accessory), 1);
        assert_eq!(macos_ns_activation_policy(MacosActivationPolicy::Prohibited), 2);
        assert_eq!(
            macos_settings_activation_policy(true, true),
            MacosActivationPolicy::Accessory
        );
        assert_eq!(
            macos_settings_activation_policy(true, false),
            MacosActivationPolicy::Accessory
        );
        assert_eq!(
            macos_settings_activation_policy(false, true),
            MacosActivationPolicy::Regular
        );
        assert_eq!(
            macos_settings_activation_policy(false, false),
            MacosActivationPolicy::Accessory
        );
        let macos = include_str!("macos.rs");
        assert!(
            macos.contains("macos_ns_activation_policy") && macos.contains("macos_settings_activation_policy"),
            "AppKit host must use the shared activation-policy mapping"
        );
        assert!(
            macos.contains("macos_overlay_level_for_terminal"),
            "iTerm Quake window level must go through the shared overlay policy"
        );
        assert!(
            !macos.contains("None | Some(0) =>"),
            "do not fork None|Some(0) → floating back into macos.rs"
        );
        assert!(
            macos.contains("macos_stores_focused_window_at_level")
                && macos.contains("macos_overlay_enabled_for_focus")
                && macos.contains("MACOS_AX_MESSAGING_TIMEOUT_SECS"),
            "AppKit host must use the shared overlay-enable / AX timeout / level-0 follow policy"
        );
        assert!(
            !macos.contains("if level == Some(0)"),
            "do not fork CGWindowLevel 0 as a literal beside macos_stores_focused_window_at_level"
        );
        assert!(
            !macos.contains("AXUIElementSetMessagingTimeout(AXUIElementCreateSystemWide(), 0.25)"),
            "do not fork the 0.25s AX timeout beside MACOS_AX_MESSAGING_TIMEOUT_SECS"
        );
    }

    #[test]
    fn macos_overlay_enable_does_not_require_a_restart_stamp() {
        let base = MacosOverlayEnable {
            is_known_terminal: true,
            integration_disabled: false,
            supports_ime: false,
            ime_enabled: false,
            autocomplete_disabled: false,
            accessibility_enabled: true,
        };
        assert!(!macos_overlay_enabled_for_focus(MacosOverlayEnable {
            is_known_terminal: false,
            ime_enabled: true,
            ..base
        }));
        assert!(!macos_overlay_enabled_for_focus(MacosOverlayEnable {
            integration_disabled: true,
            ime_enabled: true,
            ..base
        }));
        assert!(!macos_overlay_enabled_for_focus(MacosOverlayEnable {
            supports_ime: true,
            ime_enabled: false,
            ..base
        }));
        assert!(macos_overlay_enabled_for_focus(MacosOverlayEnable {
            supports_ime: true,
            ime_enabled: true,
            ..base
        }));
        assert!(macos_overlay_enabled_for_focus(base));
        assert!(!macos_overlay_enabled_for_focus(MacosOverlayEnable {
            ime_enabled: true,
            autocomplete_disabled: true,
            ..base
        }));
        assert!(!macos_overlay_enabled_for_focus(MacosOverlayEnable {
            ime_enabled: true,
            accessibility_enabled: false,
            ..base
        }));
        let macos = include_str!("macos.rs");
        assert!(
            !macos.contains("needs_restart") && macos.contains("MacosOverlayEnable"),
            "do not restore the per-process IME restart stamp"
        );
        assert!(
            macos.contains("macos_overlay_enabled_for_focus("),
            "focus-changed SetEnabled must go through the shared policy"
        );
    }

    #[test]
    fn macos_follows_ax_only_at_normal_window_level() {
        assert!(macos_stores_focused_window_at_level(Some(0)));
        assert!(!macos_stores_focused_window_at_level(None));
        assert!(!macos_stores_focused_window_at_level(Some(3)));
        assert!(!macos_stores_focused_window_at_level(Some(25)));
        assert_eq!(MACOS_AX_MESSAGING_TIMEOUT_SECS, 0.25);
    }

    #[test]
    fn macos_keystroke_reread_of_ax_still_respects_disable_flags() {
        assert!(macos_overlay_enable_from_live_ax(true, true, false));
        assert!(!macos_overlay_enable_from_live_ax(false, true, false));
        assert!(!macos_overlay_enable_from_live_ax(true, false, false));
        assert!(!macos_overlay_enable_from_live_ax(true, true, true));
        assert!(autocomplete_may_run(false, true));
        assert!(!autocomplete_may_run(true, true));
        assert!(!autocomplete_may_run(false, false));
        assert!(permission_gate_requires_ax_and_ime(true));
        assert!(!permission_gate_requires_ax_and_ime(false));
        let overlay = include_str!("../overlay.rs");
        assert!(
            overlay.contains("macos_overlay_enable_from_live_ax"),
            "the first keystroke after an AX grant must re-read through the shared helper"
        );
        let host = include_str!("../gpui_host.rs");
        assert!(
            host.contains("autocomplete_may_run"),
            "ReloadSettings must use the shared disable/AX gate"
        );
    }

    #[test]
    fn macos_ax_caret_rejects_a_selection_and_a_zero_box() {
        assert!(macos_ax_caret_is_usable(0, 0.0, 16.0));
        assert!(macos_ax_caret_is_usable(1, 8.0, 16.0));
        assert!(
            macos_ax_caret_is_usable(1, 0.0, 16.0),
            "zero-width insertion point is still a caret"
        );
        assert!(
            !macos_ax_caret_is_usable(2, 8.0, 16.0),
            "copy/paste selection is not a caret"
        );
        assert!(
            !macos_ax_caret_is_usable(0, 0.0, 0.0),
            "0×0 AX box flashes the overlay in the corner"
        );
        assert_eq!(MACOS_AX_DEFAULT_CARET_WIDTH, 10.0);
        let macos = include_str!("macos.rs");
        assert!(
            macos.contains("macos_ax_caret_is_usable") && macos.contains("MACOS_AX_DEFAULT_CARET_WIDTH"),
            "AX host must apply the shared caret-usable gate and default width"
        );
        assert!(
            !macos.contains("pub const DEFAULT_CARET_WIDTH"),
            "do not keep a second default caret width in macos.rs"
        );
        let ax = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../macos-utils/src/caret_position.rs"
        ));
        assert!(
            !ax.contains("selected_text_range.length > 1") && !ax.contains("width == 0.0 && height == 0.0"),
            "caret_position.rs reports AX success; overlay usability lives in caret.rs"
        );
        assert!(
            ax.contains("selected_length") && ax.contains("width: select_rect.size.width"),
            "AX helper must hand the range length and bounds size to the shared gate"
        );
    }

    #[test]
    fn ime_caret_request_notification_is_shared_with_the_helper() {
        assert_eq!(
            MACOS_IME_CARET_REQUEST_NOTIFICATION,
            "com.amazon.codewhisperer.edit_buffer_updated"
        );
        let macos = include_str!("macos.rs");
        assert!(
            macos.contains("MACOS_IME_CARET_REQUEST_NOTIFICATION")
                && macos.contains(MACOS_IME_CARET_REQUEST_NOTIFICATION),
            "desktop must post the shared IME caret-request name"
        );
        let imk = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_input_method/src/imk.rs"));
        assert!(
            imk.contains(MACOS_IME_CARET_REQUEST_NOTIFICATION),
            "IME helper must listen for the same leftover Amazon Q notification"
        );
        let wire = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_input_method/src/wire.rs"));
        assert!(
            wire.contains("CARET_REQUEST_NOTIFICATION") && wire.contains(MACOS_IME_CARET_REQUEST_NOTIFICATION),
            "IME wire policy must name the same notification"
        );
    }

    #[test]
    fn empty_screens_park_except_macos_top_left() {
        assert!(overlay_parks_caret_when_screens_empty(false, Origin::TopLeft));
        assert!(overlay_parks_caret_when_screens_empty(false, Origin::BottomLeft));
        assert!(!overlay_parks_caret_when_screens_empty(true, Origin::TopLeft));
        assert!(overlay_parks_caret_when_screens_empty(true, Origin::BottomLeft));
        let overlay = include_str!("../overlay.rs");
        assert!(
            overlay.contains("overlay_parks_caret_when_screens_empty(cfg!(target_os = \"macos\")"),
            "apply_position / layout_overlay must park through the shared empty-screens policy"
        );
        assert!(
            !overlay.contains(
                "#[cfg(not(target_os = \"macos\"))]\n        if matches!(position, WindowPosition::RelativeToCaret"
            ),
            "do not cfg-gate the empty-screens park beside the shared helper"
        );
    }

    #[test]
    fn ime_terminals_keep_the_overlay_on_element_change() {
        assert!(!hide_overlay_on_element_change("io.appmakes.otty"));
        assert!(!hide_overlay_on_element_change("com.mitchellh.ghostty"));
        assert!(!hide_overlay_on_element_change("net.kovidgoyal.kitty"));
        let macos = include_str!("macos.rs");
        assert!(
            macos.contains("hide_overlay_on_element_change"),
            "macos AX host still calls the shared policy"
        );
        assert!(
            !macos.contains("fn hide_overlay_on_element_change"),
            "do not fork the IME vs AX element-change policy back into macos.rs"
        );
    }

    #[test]
    fn ax_terminals_still_hide_on_element_change() {
        assert!(hide_overlay_on_element_change("com.googlecode.iterm2"));
        assert!(hide_overlay_on_element_change("com.apple.Terminal"));
        assert!(hide_overlay_on_element_change("com.microsoft.VSCode"));
    }

    #[test]
    fn null_ibus_rect_is_not_a_caret() {
        assert!(!ibus_rect_is_usable(0, 0, 0, 0));
        assert!(caret_from_ibus_absolute(0, 0, 0, 0, PositioningKind::Physical).is_none());
        assert!(caret_from_ibus_relative(0, 0, 0, 0, 10, 20, 1.0).is_none());
    }

    #[test]
    fn absolute_physical_caret_keeps_screen_space() {
        let caret = caret_from_ibus_absolute(100, 200, 8, 16, PositioningKind::Physical).unwrap();
        assert_eq!(caret.origin, Origin::TopLeft);
        match caret.position {
            Position::Physical(p) => {
                assert_eq!(p.x, 100);
                assert_eq!(p.y, 200);
            },
            other => panic!("expected physical position, got {other:?}"),
        }
        match caret.size {
            Size::Physical(s) => {
                assert_eq!(s.width, 8);
                assert_eq!(s.height, 16);
            },
            other => panic!("expected physical size, got {other:?}"),
        }
    }

    #[test]
    fn relative_caret_is_offset_from_the_window_not_used_as_the_window() {
        let caret = caret_from_ibus_relative(10, 40, 8, 16, 100, 200, 1.0).unwrap();
        match caret.position {
            Position::Logical(p) => {
                assert_eq!(p.x, 110.0);
                assert_eq!(p.y, 224.0);
            },
            other => panic!("expected logical position, got {other:?}"),
        }
    }

    #[test]
    fn relative_caret_divides_ibus_coords_by_window_scale() {
        let caret = caret_from_ibus_relative(20, 80, 16, 32, 100, 200, 2.0).unwrap();
        match caret.position {
            Position::Logical(p) => {
                assert_eq!(p.x, 110.0);
                assert_eq!(p.y, 224.0);
            },
            other => panic!("expected logical position, got {other:?}"),
        }
    }

    #[test]
    fn relative_caret_without_a_positive_scale_is_dropped() {
        assert!(caret_from_ibus_relative(10, 40, 8, 16, 100, 200, 0.0).is_none());
        assert!(caret_from_ibus_relative(10, 40, 8, 16, 100, 200, f32::NAN).is_none());
    }

    #[test]
    fn top_left_origin_does_not_need_a_screen_list() {
        assert!(!caret_origin_needs_screens(Origin::TopLeft));
        assert!(caret_origin_needs_screens(Origin::BottomLeft));
    }

    #[test]
    fn atspi_character_extents_are_a_physical_caret() {
        let caret = caret_from_atspi_extents(80, 90, 8, 18).unwrap();
        assert_eq!(caret.origin, Origin::TopLeft);
        match caret.position {
            Position::Physical(p) => {
                assert_eq!(p.x, 80);
                assert_eq!(p.y, 90);
            },
            other => panic!("expected physical position, got {other:?}"),
        }
        assert!(caret_from_atspi_extents(80, 90, 8, 0).is_none());
    }

    #[test]
    fn atspi_role_numbers_match_the_spec() {
        assert_eq!(ATSPI_ROLE_FRAME, 23);
        assert_eq!(ATSPI_ROLE_WINDOW, 69);
        assert_eq!(ATSPI_ROLE_APPLICATION, 75);
    }

    #[test]
    fn atspi_state_changed_only_reacts_to_focus_gained() {
        assert!(atspi_state_changed_is_focus_gained("focused", 1));
        assert!(!atspi_state_changed_is_focus_gained("focused", 0));
        assert!(!atspi_state_changed_is_focus_gained("checked", 1));
    }

    #[test]
    fn atspi_does_not_hide_for_our_overlay() {
        assert!(atspi_is_self_app("easy-complete"));
        assert!(atspi_is_self_app("Easy Complete"));
        assert!(!atspi_is_self_app("gnome-terminal-server"));
        assert!(!atspi_is_self_app("Firefox"));
    }

    #[test]
    fn atspi_yields_to_ibus_only_when_x11_terminal_and_ibus_is_up() {
        assert!(atspi_yields_to_ibus(Some(true), true));
        assert!(
            !atspi_yields_to_ibus(Some(true), false),
            "IBus subscribe failed: AT-SPI must still be able to place from extents"
        );
        assert!(!atspi_yields_to_ibus(None, true));
        assert!(!atspi_yields_to_ibus(Some(false), true));
        assert!(!atspi_yields_to_ibus(None, false));
    }

    #[test]
    fn atspi_256_name_and_caret_offset_are_properties() {
        assert_eq!(ATSPI_PROP_NAME, "Name");
        assert_eq!(ATSPI_PROP_CARET_OFFSET, "CaretOffset");
        assert_eq!(ATSPI_PROP_PARENT, "Parent");
        assert_eq!(ATSPI_IFACE_ACCESSIBLE, "org.a11y.atspi.Accessible");
        assert_eq!(ATSPI_IFACE_TEXT, "org.a11y.atspi.Text");
        assert_eq!(ATSPI_METHOD_GET_NAME, "GetName");
        assert_eq!(ATSPI_METHOD_GET_CARET_OFFSET, "GetCaretOffset");
        assert_eq!(ATSPI_METHOD_GET_PARENT, "GetParent");
        assert_eq!(ATSPI_COORD_TYPE_SCREEN, 0);
        assert_eq!(ATSPI_COORD_TYPE_WINDOW, 1);
        assert_ne!(
            ATSPI_COORD_TYPE_SCREEN, ATSPI_COORD_TYPE_WINDOW,
            "GetCharacterExtents must ask for screen coords, not a window box"
        );
    }

    #[test]
    fn archaeological_linux_and_windows_backends_are_gone() {
        let platform = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/platform");
        assert!(
            !platform.join("linux").exists(),
            "pre-GPUI platform/linux/ was deleted; do not restore it"
        );
        assert!(
            !platform.join("windows.rs").exists(),
            "pre-GPUI platform/windows.rs was deleted; do not restore it"
        );
        let platform_mod = include_str!("mod.rs");
        assert!(platform_mod.contains("mod linux_caret;"));
        assert!(platform_mod.contains("mod windows_caret;"));
        assert!(
            !platform_mod.contains("mod linux;\n") && !platform_mod.contains("mod linux; "),
            "old platform/linux/ must stay uncompiled"
        );
        assert!(
            !platform_mod.contains("mod windows;\n") && !platform_mod.contains("mod windows; "),
            "old platform/windows.rs must stay uncompiled"
        );
        let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(
            !src.join("webview").exists(),
            "webview/ was renamed to bootstrap/; do not restore the old name"
        );
        assert!(src.join("bootstrap/mod.rs").exists());
        let bootstrap = include_str!("../bootstrap/mod.rs");
        assert!(
            bootstrap.contains("pub struct AppRuntime"),
            "desktop bootstrap type is AppRuntime"
        );
        assert!(
            !bootstrap.contains("struct WebviewManager"),
            "WKWebView host name must stay gone"
        );
        assert!(
            !src.join("bootstrap/notification.rs").exists(),
            "WebView window notification subscription map is gone"
        );
        assert!(
            !bootstrap.contains("WryIdMap") && !bootstrap.contains("FigIdMap"),
            "WKWebView window-id map is gone with the WebView host"
        );
        assert!(
            !bootstrap.contains("api_handler_tx"),
            "disconnected WebView JS handler channel is gone"
        );
        assert!(
            bootstrap.contains("SETTINGS_ID") && !bootstrap.contains("DASHBOARD_ID"),
            "settings window id is SETTINGS_ID, not the WebView dashboard name"
        );
        assert_eq!(crate::AUTOCOMPLETE_WINDOW_TITLE, ec_gpui::OVERLAY_WINDOW_TITLE);
        assert_eq!(ec_gpui::OVERLAY_WINDOW_TITLE, "Easy Complete");
        assert!(
            !ec_gpui::OVERLAY_WINDOW_TITLE.contains("Fig"),
            "overlay title is a product string"
        );
        let update = include_str!("../update.rs");
        assert!(
            !update.contains("show_webview") && !update.contains("WryId"),
            "Sparkle prompt flag must not keep the WebView name"
        );
        let overlay = include_str!("../overlay.rs");
        assert!(
            overlay.contains("fn caret_y_in_screen_space"),
            "caret Y conversion is screen space on every OS"
        );
        assert!(
            !overlay.contains("quartz_y_") && !overlay.contains("caret_y_in_quartz_space"),
            "overlay Y helpers must not keep quartz_y_* names"
        );
    }

    #[test]
    fn rust_linux_and_windows_ci_run_desktop_host_tests() {
        let ci = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../.github/workflows/ci.yml"));
        assert!(
            ci.contains("-p fig_log"),
            "fig_log poison recovery belongs on rust-linux and rust-windows"
        );
        assert!(
            !ci.contains("fig_desktop -- platform::caret"),
            "desktop tests must not be filtered to caret conversion only"
        );
        assert!(
            !ci.contains("fig_desktop -- overlay::"),
            "overlay placement is covered by cargo test -p fig_desktop"
        );
        assert!(
            ci.matches("cargo test --locked -p fig_desktop").count() >= 2,
            "rust-linux and rust-windows both run the fig_desktop suite"
        );
        assert!(
            ci.matches("cargo test --locked -p ec_gpui").count() >= 2,
            "rust-linux and rust-windows both run the ec_gpui suite (macos_overlay / windows_overlay)"
        );
        assert!(
            ci.matches("-p fig_input_method").count() >= 2,
            "IME terminals / wire / TISDisable pins belong on rust-linux and rust-windows"
        );
        assert!(
            ci.matches("-p ec_hitoolbox").count() >= 2,
            "HIToolbox palette rewrite policy belongs on rust-linux and rust-windows"
        );
    }

    #[test]
    fn leftover_crates_are_not_default_workspace_members() {
        let workspace = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"));
        assert!(
            workspace
                .lines()
                .any(|line| line.trim() == "default-members = [\"crates/ec_cli\"]"),
            "default-members must stay only the CLI"
        );

        let linux = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/build-linux.sh"));
        let macos = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/build-app.sh"));
        assert!(
            !linux.contains("ec_overlay_spike") && !macos.contains("ec_overlay_spike"),
            "dist profiles must not ship the overlay spike"
        );
        assert!(
            linux.contains("-p fig_desktop") && macos.contains("-p fig_desktop"),
            "dist still builds fig_desktop"
        );
        assert!(
            !workspace.contains("fig_desktop_api")
                && !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_desktop_api")).exists(),
            "install dispatcher lives in fig_desktop; the leftover API crate is gone"
        );
        assert!(
            !std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../fig_integrations/src/gnome_extension.rs"
            ))
            .exists(),
            "GNOME Shell extension integration is not a Linux v1 surface"
        );
        let install = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_install/src/common.rs"));
        assert!(
            !install.contains("GNOME_SHELL_EXTENSION"),
            "InstallComponents must not keep a GNOME Shell extension bit"
        );
        let util_lib = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_util/src/lib.rs"));
        assert!(
            util_lib.contains("pub mod launchd_plist")
                && !util_lib.contains("#[cfg(target_os = \"macos\")]\npub mod launchd_plist"),
            "LaunchAgent plist XML is compiled on every OS so Linux CI pins --is-startup"
        );
        assert!(
            !workspace.contains("features = [\"full\"]")
                && workspace.contains("rt-multi-thread")
                && workspace.contains("\"signal\""),
            "workspace tokio lists used features instead of full"
        );
        assert!(
            !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../figterm/src/inline")).exists()
                && !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/src/cli/inline.rs")).exists()
                && !std::path::Path::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../ec_cli/src/cli/internal/inline_shell_completion.rs"
                ))
                .exists()
                && !std::path::Path::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../fig_integrations/src/shell/inline_shell_completion"
                ))
                .exists(),
            "Amazon Q inline shell completion is gone; proto handlers stay no-ops"
        );
        let figterm_message = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../figterm/src/message.rs"));
        assert!(
            figterm_message.contains("FigtermRequest::InlineShellCompletion(_)")
                && !figterm_message.contains("crate::inline"),
            "figterm still drops inline-shell-completion proto requests without restoring the module"
        );

        let desktop_manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        assert!(
            !desktop_manifest.contains("tao"),
            "GPUI host must not keep the wry/tao windowing crate"
        );
        let event = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/event.rs"));
        assert!(
            event.contains("Quit")
                && event.contains("ThemeChanged")
                && !event.contains("WindowEventAll")
                && !event.contains("SetTheme")
                && !event.contains("dry_run")
                && !event.contains("WindowGeometryResult"),
            "desktop events are GPUI quit/theme, not tao ControlFlow or WebView geometry RPC"
        );
        assert!(
            !include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/utils.rs")).contains("fig.png"),
            "desktop must not look up the leftover fig.png XDG icon"
        );
        assert!(
            !workspace.contains("aws-smithy") && !workspace.contains("aws-types"),
            "Amazon Q smithy workspace deps are gone"
        );
        let env = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_os_shim/src/env.rs"));
        assert!(
            !env.contains("amazon_q_sigv4")
                && !env.contains("AMAZON_Q_CHAT_SHELL")
                && !env.contains("Q_CLI_CLIENT_APPLICATION")
                && !env.contains("Q_BACKEND")
                && !env.contains("Q_USE_SENDMESSAGE")
                && !env.contains("Q_CUSTOM_CERT"),
            "unused Amazon Q env helpers are gone; keep Q_TERM / Q_PARENT / Q_LOG_LEVEL"
        );
        assert!(
            !std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../ec_cli/src/util/region_check.rs"
            ))
            .exists()
                && !std::path::Path::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../ec_cli/tests/chat_response_stubs"
                ))
                .exists(),
            "GovCloud region_check and Amazon Q chat fixtures are gone"
        );
        assert!(
            !include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_util/src/lib.rs"))
                .contains("search_xdg_data_dirs"),
            "fig_util must not keep the fig.png XDG lookup"
        );
        let directories = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_util/src/directories.rs"));
        assert!(
            !directories.contains("chat_global_context_path")
                && !directories.contains("chat_profiles_dir")
                && !directories.contains("amazonq"),
            "Amazon Q chat config paths are gone"
        );
        let init = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/src/cli/init.rs"));
        let init_production = init.split("#[cfg(test)]").next().expect("init production");
        assert!(
            !init_production.contains("immediateLogin")
                && !init_production.contains("auth-watcher.logged-in")
                && !init_production.contains("login_prompt_code")
                && !init_production.contains("fig app onboarding"),
            "ec init must not inject Amazon Q login or onboarding"
        );
        let app = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/src/cli/app/mod.rs"));
        assert!(
            !app.contains("AppSubcommand") && !app.contains("user.onboarding") && app.contains("fn restart_desktop"),
            "CLI app module keeps restart_desktop and drops Amazon Q onboarding/prompts"
        );
        let cli_mod = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/src/cli/mod.rs"));
        assert!(
            cli_mod.contains("\nmod app;") && !cli_mod.contains("pub mod app"),
            "app must stay crate-private so dead Amazon Q subcommands cannot hide behind pub mod"
        );
        assert!(
            !std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../macos-utils/accessibility-master/aq"
            ))
            .exists(),
            "Amazon Q accessibility query demo is not a workspace crate"
        );
        assert!(
            !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/not-logged-in.png")).exists()
                && !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/not-logged-in-light.png"))
                    .exists(),
            "signed-out tray icons went with the auth tray"
        );
        let consts = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_util/src/consts.rs"));
        let tauri_name = ["TAURI", "_PRODUCT_NAME"].concat();
        let minimal_name = ["CLI_BINARY_NAME", "_MINIMAL"].concat();
        assert!(
            !consts.contains(&tauri_name) && !consts.contains(&minimal_name),
            "Tauri product-name and unused ec-minimal binary name are gone"
        );
        assert!(
            consts.contains("EC_BUILD_HASH") && consts.contains("AMAZON_Q_BUILD_HASH"),
            "build metadata prefers EC_BUILD_* and still accepts leftover AMAZON_Q_BUILD_*"
        );
        assert!(
            !directories.contains(&tauri_name) && directories.contains("lib/{PRODUCT_NAME}"),
            "AppImage resources use PRODUCT_NAME, not a Tauri layout constant"
        );
        let cli_manifest = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/Cargo.toml"));
        let clipboard = ["ar", "board"].concat();
        let case_crate = ["convert", "_case"].concat();
        let bench_crate = ["criter", "ion"].concat();
        assert!(
            !cli_manifest.contains(&clipboard)
                && !cli_manifest.contains(&case_crate)
                && !cli_manifest.contains(&bench_crate),
            "CLI must not keep unused clipboard / case / bench crates"
        );
        assert!(
            !workspace.contains(&case_crate),
            "workspace convert_case is unused after dropping the CLI leftover"
        );
        assert!(
            !desktop_manifest.contains("[package.metadata.bundle]"),
            "desktop dist is build-app.sh, not leftover tauri-bundler metadata"
        );
        let internal = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../ec_cli/src/cli/internal/mod.rs"
        ));
        let internal_production = internal.split("#[cfg(test)]").next().expect("internal production");
        assert!(
            !internal_production.contains("IbusBootstrap") && !internal_production.contains("ibus-daemon"),
            "CLI must not launch ibus-daemon; linux_caret owns IBus"
        );
        assert!(
            cli_mod.contains("\nmod internal;") && !cli_mod.contains("pub mod internal"),
            "internal must stay crate-private so leftover Amazon Q subcommands cannot hide behind pub mod"
        );
        assert!(
            !std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../fig_test_macro")).exists(),
            "unused fig_test_macro (broken fig_test ENVIRONMENT_LOCK) is gone"
        );
        let sqlite = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../fig_settings/src/sqlite/mod.rs"
        ));
        let sqlite_production = sqlite.split("#[cfg(test)]").next().expect("sqlite production");
        assert!(
            !sqlite_production.contains("get_auth_value")
                && !sqlite_production.contains("AUTH_TABLE_NAME")
                && sqlite.contains("006_drop_auth_table"),
            "Amazon Q auth_kv APIs are gone; 005 stays in history and 006 drops the table"
        );
        let doctor = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/src/cli/doctor/mod.rs"));
        let doctor_production = doctor.split("#[cfg(test)]").next().expect("doctor production");
        assert!(
            !doctor_production.contains("#![allow(dead_code)]")
                && !doctor_production.contains("HyperIntegrationCheck")
                && !doctor_production.contains("fig-hyper-integration")
                && !doctor_production.contains("PluginDevModeCheck")
                && !doctor_production.contains("struct FigBinCheck")
                && !doctor_production.contains("analytics_event_name"),
            "doctor no longer hides leftover Fig/Hyper/plugin checks behind allow(dead_code)"
        );
        let diagnostics = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/src/cli/diagnostics.rs"));
        let diagnostics_production = diagnostics
            .split("#[cfg(test)]")
            .next()
            .expect("diagnostics production");
        assert!(
            !diagnostics_production.contains("verify_integration")
                && !diagnostics_production.contains("TerminalIntegration"),
            "terminal-integration verify IPC was only used by leftover Hyper/VSCode doctor checks"
        );
    }

    #[test]
    fn win32_client_caret_is_physical_top_left_on_the_screen() {
        let caret = caret_from_win32_client_caret(4, 8, 12, 24, 104, 208).unwrap();
        assert_eq!(caret.origin, Origin::TopLeft);
        match caret.position {
            Position::Physical(p) => {
                assert_eq!(p.x, 104);
                assert_eq!(p.y, 208);
            },
            other => panic!("expected physical position, got {other:?}"),
        }
        match caret.size {
            Size::Physical(s) => {
                assert_eq!(s.width, 8);
                assert_eq!(s.height, 16);
            },
            other => panic!("expected physical size, got {other:?}"),
        }
    }

    #[test]
    fn win32_zero_height_caret_is_dropped_zero_width_is_widened() {
        assert!(caret_from_win32_client_caret(0, 0, 0, 0, 10, 20).is_none());
        let caret = caret_from_win32_client_caret(10, 10, 10, 26, 100, 200).unwrap();
        match caret.size {
            Size::Physical(s) => {
                assert_eq!(s.width, 1);
                assert_eq!(s.height, 16);
            },
            other => panic!("expected physical size, got {other:?}"),
        }
    }

    #[test]
    fn win32_missing_hwnd_caret_is_not_a_window_rect() {
        assert!(win32_caret_from_gui_thread(false, 0, 0, 80, 24, 10, 20).is_none());
        let caret = win32_caret_from_gui_thread(true, 4, 8, 12, 24, 104, 208).unwrap();
        match caret.position {
            Position::Physical(p) => {
                assert_eq!(p.x, 104);
                assert_eq!(p.y, 208);
            },
            other => panic!("expected physical position, got {other:?}"),
        }
    }

    #[test]
    fn win32_lost_caret_hides_the_overlay() {
        assert_eq!(
            win32_caret_poll_action(false, false),
            (false, Win32CaretPollAction::Idle)
        );
        assert_eq!(win32_caret_poll_action(false, true), (true, Win32CaretPollAction::Send));
        assert_eq!(win32_caret_poll_action(true, true), (true, Win32CaretPollAction::Send));
        assert_eq!(
            win32_caret_poll_action(true, false),
            (false, Win32CaretPollAction::Hide)
        );
    }

    #[test]
    fn windows_caret_host_uses_the_shared_gui_thread_policy() {
        let src = include_str!("windows_caret.rs");
        assert!(
            src.contains("win32_caret_from_gui_thread"),
            "Win32 host must go through the shared hwndCaret gate"
        );
        assert!(
            src.contains("win32_caret_poll_action"),
            "lost-caret Hide must use the shared poll action"
        );
        assert!(
            src.contains("WindowPosition::RelativeToCaret"),
            "Win32 place is a caret, not a window rect"
        );
        assert!(
            !src.contains("GetWindowRect") && !src.contains("PositionRelativeToRect"),
            "missing hwndCaret must not fall back to the console window"
        );
        assert!(
            !src.contains("caret_from_win32_client_caret("),
            "do not fork the hwndCaret gate; call win32_caret_from_gui_thread"
        );
    }
}
