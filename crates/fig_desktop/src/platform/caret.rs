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
use tao::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Size};

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

/// tao / NSApplication activation policy, without AppKit.
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
/// Unknown future tao variants map to Accessory, matching the previous `_ => 1`.
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
            !workspace.contains("features = [\"full\"]")
                && workspace.contains("rt-multi-thread")
                && workspace.contains("\"signal\""),
            "workspace tokio lists used features instead of full"
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
