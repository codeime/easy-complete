//! Shared GPUI overlay window and suggestion list used by the spike binary
//! and by `fig_desktop`.

mod icons;
#[cfg(target_os = "linux")]
mod linux;
/// X11 overlay hint policy. Compiled on every OS so Linux CI pins D3;
/// `linux.rs` is still `cfg(linux)` and talks to the X server.
mod linux_overlay;
mod list;
#[cfg(target_os = "macos")]
mod macos;
/// macOS overlay policy. Compiled on every OS so Linux CI pins
/// `NSScreen.screens[0]` (not `mainScreen`), the Quartz→Cocoa Y flip, and
/// the frame-echo schedule. `macos.rs` is still `cfg(macos)` and talks to AppKit.
mod macos_overlay;
mod overlay;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform_stub;
mod theme;
#[cfg(target_os = "windows")]
mod windows;
/// HWND / SetWindowPos policy. Compiled on every OS so Linux CI pins F5;
/// `windows.rs` is still `cfg(windows)` and issues the real call.
mod windows_overlay;

#[cfg(target_os = "linux")]
pub use linux::{
    harden_overlay_window, harden_overlay_window_handle, harden_overlay_window_titled,
    invalidate_cached_overlay_x_window, overlay_placement_scale, overlay_screens, park_overlay_window_handle,
    park_overlay_window_titled, polish_overlay_window_titled, screen_y_to_frame_y, set_overlay_frame_handle,
    set_overlay_frame_titled, set_overlay_visible_handle, set_overlay_visible_titled, set_overlay_window_level,
    set_overlay_window_level_for_title, system_appearance_is_dark,
};
pub use linux_overlay::{OverlayX11Hints, overlay_x11_activates, overlay_x11_hints, overlay_x11_place_changes_size};
pub use list::{
    ClickInsert, DEFAULT_FONT_SIZE, DEFAULT_MAX_LIST_HEIGHT, DEFAULT_ROW_HEIGHT, DEFAULT_WIDTH, DESCRIPTION_HEIGHT,
    DEV_BANNER_HEIGHT, OverlayTheme, POPOUT_WIDTH, SuggestionItem, SuggestionList, TabPrefix, common_prefix_for,
    kind_label, layout_gap, layout_pad, longest_common_prefix, match_prefix_bytes, overlay_content_size,
    overlay_content_size_with_context, selection_identity, tab_prefix_insertion,
};
#[cfg(target_os = "macos")]
pub use macos::{
    harden_overlay_window, harden_overlay_window_handle, harden_overlay_window_titled,
    invalidate_cached_overlay_x_window, overlay_placement_scale, overlay_screens, park_overlay_window_handle,
    park_overlay_window_titled, polish_overlay_window_titled, set_overlay_frame_handle, set_overlay_frame_titled,
    set_overlay_visible_handle, set_overlay_visible_titled, set_overlay_window_level,
    set_overlay_window_level_for_title, system_appearance_is_dark,
};
pub use macos_overlay::{
    macos_overlay_activates, macos_overlay_anchors_to_main_screen, macos_primary_screen_index,
    quartz_y_to_cocoa_frame_y,
};
pub use overlay::{
    OVERLAY_WINDOW_TITLE, OverlayHandle, OverlayState, open_overlay_window, open_overlay_window_with_visibility,
    overlay_window_options, park_overlay_handle, position_overlay,
};
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub use platform_stub::{
    harden_overlay_window, harden_overlay_window_handle, harden_overlay_window_titled,
    invalidate_cached_overlay_x_window, overlay_placement_scale, overlay_screens, park_overlay_window_handle,
    park_overlay_window_titled, polish_overlay_window_titled, screen_y_to_frame_y, set_overlay_frame_handle,
    set_overlay_frame_titled, set_overlay_visible_handle, set_overlay_visible_titled, set_overlay_window_level,
    set_overlay_window_level_for_title, system_appearance_is_dark,
};
pub use theme::{parse_color, theme_from_json};
#[cfg(target_os = "windows")]
pub use windows::{
    harden_overlay_window, harden_overlay_window_handle, harden_overlay_window_titled,
    invalidate_cached_overlay_x_window, overlay_placement_scale, overlay_screens, park_overlay_window_handle,
    park_overlay_window_titled, polish_overlay_window_titled, screen_y_to_frame_y, set_overlay_frame_handle,
    set_overlay_frame_titled, set_overlay_visible_handle, set_overlay_visible_titled, set_overlay_window_level,
    set_overlay_window_level_for_title, system_appearance_is_dark,
};
