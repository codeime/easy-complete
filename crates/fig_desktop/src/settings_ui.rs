//! Native GPUI settings window. Replaces the dashboard WKWebView.

#![allow(unexpected_cfgs)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, MouseButton, SharedString, TitlebarOptions, WindowBounds,
    WindowHandle, WindowOptions, div, point, px, rgb, size,
};
use tracing::error;

use crate::EventLoopProxy;
use crate::event::{Event, WindowEvent};
use crate::permissions::{self, PermId, PermReady, PermissionSnapshot};
use crate::platform::PlatformBoundEvent;
use crate::webview::DASHBOARD_ID;

pub const SETTINGS_WINDOW_TITLE: &str = "Settings";

static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)] // reserved for callers that gate on settings visibility
pub fn is_open() -> bool {
    SETTINGS_OPEN.load(Ordering::Relaxed)
}

const SIDEBAR_W: f32 = 226.0;
const WIN_W: f32 = 820.0;
const WIN_H: f32 = 640.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Appearance,
    Behavior,
    About,
}

#[derive(Clone, Copy)]
struct Chrome {
    bg: u32,
    sidebar: u32,
    sidebar_border: u32,
    text: u32,
    muted: u32,
    card: u32,
    separator: u32,
    accent: u32,
    track_off: u32,
}

impl Chrome {
    fn current() -> Self {
        if ec_gpui::system_appearance_is_dark() {
            Self {
                bg: 0x1c1c1e,
                sidebar: 0x161618,
                sidebar_border: 0x2c2c2e,
                text: 0xf5f5f7,
                muted: 0x8e8e93,
                card: 0x2c2c2e,
                separator: 0x3a3a3c,
                accent: 0x0a84ff,
                track_off: 0x48484a,
            }
        } else {
            Self {
                bg: 0xf5f5f7,
                sidebar: 0xe8e8ed,
                sidebar_border: 0xd4d4d8,
                text: 0x1d1d1f,
                muted: 0x6e6e73,
                card: 0xffffff,
                separator: 0xe5e5ea,
                accent: 0x007aff,
                track_off: 0xd1d1d6,
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeAppearance {
    System,
    Light,
    Dark,
}

struct ThemeSwatch {
    id: &'static str,
    label_en: &'static str,
    label_zh: &'static str,
    appearance: ThemeAppearance,
    bg: u32,
    text: u32,
    selection: u32,
    accent: u32,
}

const THEMES: &[ThemeSwatch] = &[
    ThemeSwatch {
        id: "system",
        label_en: "System",
        label_zh: "跟随系统",
        appearance: ThemeAppearance::System,
        bg: 0x1c1c1c,
        text: 0xd0d0d0,
        selection: 0x007aff,
        accent: 0x007aff,
    },
    ThemeSwatch {
        id: "light",
        label_en: "Light",
        label_zh: "浅色",
        appearance: ThemeAppearance::Light,
        bg: 0xfefefe,
        text: 0x070707,
        selection: 0x2969da,
        accent: 0xfff899,
    },
    ThemeSwatch {
        id: "github-light",
        label_en: "GitHub Light",
        label_zh: "GitHub Light",
        appearance: ThemeAppearance::Light,
        bg: 0xffffff,
        text: 0x24292f,
        selection: 0x0969da,
        accent: 0xfff8c5,
    },
    ThemeSwatch {
        id: "claude-light",
        label_en: "Claude Light",
        label_zh: "Claude Light",
        appearance: ThemeAppearance::Light,
        bg: 0xf3f1e9,
        text: 0x1a1917,
        selection: 0xefe5db,
        accent: 0xcc785c,
    },
    ThemeSwatch {
        id: "catppuccin-latte",
        label_en: "Catppuccin Latte",
        label_zh: "Catppuccin Latte",
        appearance: ThemeAppearance::Light,
        bg: 0xeff1f5,
        text: 0x4c4f69,
        selection: 0x1e66f5,
        accent: 0x8839ef,
    },
    ThemeSwatch {
        id: "dark",
        label_en: "Dark",
        label_zh: "深色",
        appearance: ThemeAppearance::Dark,
        bg: 0x303030,
        text: 0xb4b4b4,
        selection: 0x1e5ac7,
        accent: 0x5f5938,
    },
    ThemeSwatch {
        id: "github-dark",
        label_en: "GitHub Dark",
        label_zh: "GitHub Dark",
        appearance: ThemeAppearance::Dark,
        bg: 0x0d1117,
        text: 0xc9d1d9,
        selection: 0x1f6feb,
        accent: 0x388bfd,
    },
    ThemeSwatch {
        id: "claude-dark",
        label_en: "Claude Dark",
        label_zh: "Claude Dark",
        appearance: ThemeAppearance::Dark,
        bg: 0x262624,
        text: 0xf0eee6,
        selection: 0x3d3d3a,
        accent: 0xcc785c,
    },
    ThemeSwatch {
        id: "nord",
        label_en: "Nord",
        label_zh: "Nord",
        appearance: ThemeAppearance::Dark,
        bg: 0x2e3440,
        text: 0xd8dee9,
        selection: 0x5e81ac,
        accent: 0x88c0d0,
    },
    ThemeSwatch {
        id: "gruvbox-dark",
        label_en: "Gruvbox Dark",
        label_zh: "Gruvbox Dark",
        appearance: ThemeAppearance::Dark,
        bg: 0x282828,
        text: 0xebdbb2,
        selection: 0x458588,
        accent: 0xd79921,
    },
    ThemeSwatch {
        id: "one-dark",
        label_en: "One Dark",
        label_zh: "One Dark",
        appearance: ThemeAppearance::Dark,
        bg: 0x282c34,
        text: 0xabb2bf,
        selection: 0x528bff,
        accent: 0x98c379,
    },
    ThemeSwatch {
        id: "tokyo-night",
        label_en: "Tokyo Night",
        label_zh: "Tokyo Night",
        appearance: ThemeAppearance::Dark,
        bg: 0x1a1b26,
        text: 0xa9b1d6,
        selection: 0x364a82,
        accent: 0xbb9af7,
    },
];

fn shows_permission_gate(gate: &PermissionSnapshot) -> bool {
    !gate.still_checking() && !gate.all_ready()
}

const FONTS: &[&str] = &["Menlo", "Monaco", "Hack", "SF Mono", "JetBrains Mono"];

pub struct SettingsWindow {
    section: Section,
    proxy: EventLoopProxy,
    gate: PermissionSnapshot,
    repairing: Option<PermId>,
    copied_doctor: bool,
}

pub type SettingsHandle = WindowHandle<SettingsWindow>;

impl SettingsWindow {
    fn zh() -> bool {
        locale_is_zh()
    }

    fn set_bool(&mut self, key: &str, value: bool, cx: &mut Context<'_, Self>) {
        if let Err(err) = fig_settings::settings::set_value(key, value) {
            error!(%err, key, "Failed to write setting");
        }
        self.proxy.send_event(Event::ReloadCredentials).ok();
        cx.notify();
    }

    fn set_string(&mut self, key: &str, value: impl Into<serde_json::Value>, cx: &mut Context<'_, Self>) {
        if let Err(err) = fig_settings::settings::set_value(key, value) {
            error!(%err, key, "Failed to write setting");
        }
        self.proxy.send_event(Event::ReloadCredentials).ok();
        cx.notify();
    }

    fn set_int(&mut self, key: &str, value: i64, cx: &mut Context<'_, Self>) {
        self.set_string(key, value, cx);
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let chrome = Chrome::current();
        let entity = cx.entity();
        let zh = Self::zh();
        let section = self.section;
        let root = div()
            .id("ec-settings")
            .flex()
            .flex_row()
            .w_full()
            .h_full()
            .bg(rgb(chrome.bg))
            .text_color(rgb(chrome.text))
            .text_size(px(13.))
            .font_family(".AppleSystemUIFont");

        // The old dashboard showed a spinner while permissions were still
        // being checked, and the gate only after a failed result. Treating
        // `Checking` as "not ready" flashes the permission page on every open.
        if shows_permission_gate(&self.gate) {
            return root.child(permission_gate_page(
                zh,
                chrome,
                self.gate.clone(),
                self.repairing,
                entity,
            ));
        }

        root.child(sidebar(section, zh, chrome, entity.clone())).child(
            div()
                .id("ec-settings-main")
                .flex_1()
                .flex()
                .flex_col()
                .pt(px(52.))
                .px(px(28.))
                .pb(px(24.))
                .overflow_y_scroll()
                .child(match section {
                    Section::Appearance => appearance_page(zh, chrome, entity.clone()).into_any_element(),
                    Section::Behavior => behavior_page(zh, chrome, entity).into_any_element(),
                    Section::About => about_page(zh, chrome, entity, self.copied_doctor).into_any_element(),
                }),
        )
    }
}

fn sidebar(section: Section, zh: bool, chrome: Chrome, entity: Entity<SettingsWindow>) -> impl IntoElement {
    let items = [
        (Section::Appearance, if zh { "外观" } else { "Appearance" }),
        (Section::Behavior, if zh { "行为" } else { "Behavior" }),
        (Section::About, if zh { "关于" } else { "About" }),
    ];
    let mut nav = div().flex().flex_col().px(px(10.)).gap(px(2.));
    for (id, label) in items {
        let active = section == id;
        let entity = entity.clone();
        nav = nav.child(
            div()
                .id(("ec-settings-nav", id as u32))
                .h(px(28.))
                .px(px(8.))
                .rounded(px(6.))
                .flex()
                .flex_row()
                .items_center()
                .cursor_pointer()
                .bg(rgb(if active { chrome.card } else { chrome.sidebar }))
                .text_color(rgb(if active { chrome.text } else { chrome.muted }))
                .font_weight(if active {
                    gpui::FontWeight::MEDIUM
                } else {
                    gpui::FontWeight::NORMAL
                })
                .child(label.to_string())
                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                    entity.update(cx, |this, cx| {
                        this.section = id;
                        cx.notify();
                    });
                }),
        );
    }
    div()
        .id("ec-settings-sidebar")
        .w(px(SIDEBAR_W))
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(chrome.sidebar))
        .border_r_1()
        .border_color(rgb(chrome.sidebar_border))
        .pt(px(44.))
        .child(
            div()
                .px(px(16.))
                .pb(px(8.))
                .text_size(px(11.))
                .text_color(rgb(chrome.muted))
                .child(if zh { "设置" } else { "Settings" }.to_string()),
        )
        .child(nav)
}

fn card(title: &str, chrome: Chrome, children: impl IntoElement) -> impl IntoElement {
    div()
        .mb(px(16.))
        .child(
            div()
                .mb(px(8.))
                .text_size(px(12.))
                .text_color(rgb(chrome.muted))
                .child(title.to_string()),
        )
        .child(
            div()
                .rounded(px(10.))
                .bg(rgb(chrome.card))
                .border_1()
                .border_color(rgb(chrome.separator))
                .child(children),
        )
}

fn stacked_row(
    label: &str,
    description: Option<&str>,
    chrome: Chrome,
    last: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    let mut body = div().px(px(16.)).py(px(12.));
    if !last {
        body = body.border_b_1().border_color(rgb(chrome.separator));
    }
    body.child(div().text_color(rgb(chrome.text)).child(label.to_string()))
        .when_some(description.map(str::to_string), |this, desc| {
            this.child(
                div()
                    .mt(px(3.))
                    .text_size(px(12.))
                    .text_color(rgb(chrome.muted))
                    .child(desc),
            )
        })
        .child(div().mt(px(10.)).w_full().child(control))
}

fn row(
    label: &str,
    description: Option<&str>,
    chrome: Chrome,
    last: bool,
    control: impl IntoElement,
) -> impl IntoElement {
    let mut body = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(16.))
        .px(px(16.))
        .py(px(12.));
    if !last {
        body = body.border_b_1().border_color(rgb(chrome.separator));
    }
    let mut left = div().flex().flex_col().flex_1().min_w(px(0.));
    left = left.child(div().text_color(rgb(chrome.text)).child(label.to_string()));
    if let Some(description) = description {
        left = left.child(
            div()
                .mt(px(3.))
                .text_size(px(12.))
                .text_color(rgb(chrome.muted))
                .child(description.to_string()),
        );
    }
    body.child(left).child(control)
}

fn toggle(id: SharedString, checked: bool, chrome: Chrome, on_click: impl Fn(&mut App) + 'static) -> impl IntoElement {
    div()
        .id(id)
        .w(px(40.))
        .h(px(24.))
        .rounded(px(12.))
        .bg(rgb(if checked { chrome.accent } else { chrome.track_off }))
        .flex()
        .flex_row()
        .items_center()
        .px(px(2.))
        .cursor_pointer()
        .child(
            div()
                .w(px(20.))
                .h(px(20.))
                .rounded(px(10.))
                .bg(rgb(0xffffff))
                .when(checked, |this| this.ml(px(16.))),
        )
        .on_mouse_down(MouseButton::Left, move |_e, _w, cx| on_click(cx))
}

fn stepper(
    id: &'static str,
    value: i64,
    min: i64,
    max: i64,
    step: i64,
    chrome: Chrome,
    on_set: impl Fn(i64, &mut App) + 'static,
) -> impl IntoElement {
    let on_set = std::rc::Rc::new(on_set);
    let dec = {
        let on_set = on_set.clone();
        move |_e: &gpui::MouseDownEvent, _w: &mut gpui::Window, cx: &mut App| {
            on_set((value - step).clamp(min, max), cx);
        }
    };
    let inc = {
        let on_set = on_set;
        move |_e: &gpui::MouseDownEvent, _w: &mut gpui::Window, cx: &mut App| {
            on_set((value + step).clamp(min, max), cx);
        }
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .id((id, 0u32))
                .w(px(22.))
                .h(px(22.))
                .rounded(px(6.))
                .bg(rgb(chrome.separator))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child("−")
                .on_mouse_down(MouseButton::Left, dec),
        )
        .child(div().min_w(px(36.)).flex().justify_center().child(value.to_string()))
        .child(
            div()
                .id((id, 1u32))
                .w(px(22.))
                .h(px(22.))
                .rounded(px(6.))
                .bg(rgb(chrome.separator))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child("+")
                .on_mouse_down(MouseButton::Left, inc),
        )
}

fn select_chips(
    id_prefix: &'static str,
    options: &[(&'static str, &'static str)],
    current: &str,
    chrome: Chrome,
    on_pick: impl Fn(&'static str, &mut App) + 'static,
) -> impl IntoElement {
    let on_pick = std::rc::Rc::new(on_pick);
    let mut row = div().flex().flex_row().flex_wrap().gap(px(6.));
    for (i, (label, value)) in options.iter().enumerate() {
        let selected = *value == current;
        let value = *value;
        let on_pick = on_pick.clone();
        row = row.child(
            div()
                .id((id_prefix, i as u32))
                .px(px(10.))
                .py(px(5.))
                .rounded(px(8.))
                .cursor_pointer()
                .bg(rgb(if selected { chrome.accent } else { chrome.separator }))
                .text_color(rgb(if selected { 0xffffff } else { chrome.text }))
                .child((*label).to_string())
                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| on_pick(value, cx)),
        );
    }
    row
}

fn theme_dot(color: u32, opacity: f32) -> impl IntoElement {
    div()
        .w(px(6.))
        .h(px(6.))
        .rounded(px(99.))
        .bg(rgb(color))
        .opacity(opacity)
}

fn theme_preview_window(swatch: &ThemeSwatch) -> impl IntoElement {
    let light = matches!(swatch.appearance, ThemeAppearance::Light | ThemeAppearance::System);
    let chrome_line = if light { 0xd0d0d6 } else { 0x3a3a3c };
    let field_opacity = if light { 0.35 } else { 0.06 };
    div()
        .rounded(px(9.))
        .bg(rgb(swatch.bg))
        .border_1()
        .border_color(rgb(chrome_line))
        .overflow_hidden()
        .child(
            div()
                .h(px(20.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .border_b_1()
                .border_color(rgb(chrome_line))
                .child(theme_dot(swatch.accent, 0.55))
                .child(theme_dot(swatch.text, 0.38))
                .child(theme_dot(swatch.text, 0.32))
                .child(
                    div()
                        .ml(px(4.))
                        .flex_1()
                        .h(px(12.))
                        .rounded(px(5.))
                        .border_1()
                        .border_color(rgb(chrome_line))
                        .opacity(0.75),
                ),
        )
        .child(
            div()
                .px(px(12.))
                .py(px(10.))
                .child(
                    div()
                        .h(px(16.))
                        .mb(px(8.))
                        .rounded(px(5.))
                        .bg(rgb(swatch.selection))
                        .opacity(0.85),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(div().w(px(8.)).h(px(8.)).rounded(px(3.)).bg(rgb(swatch.accent)))
                        .child(
                            div()
                                .h(px(8.))
                                .w(px(52.))
                                .rounded(px(4.))
                                .bg(rgb(swatch.text))
                                .opacity(0.55),
                        )
                        .child(
                            div()
                                .flex_1()
                                .h(px(16.))
                                .rounded(px(5.))
                                .border_1()
                                .border_color(rgb(chrome_line))
                                .bg(rgb(0xffffff))
                                .opacity(field_opacity),
                        ),
                )
                .child(
                    div()
                        .mt(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .h(px(6.))
                                .w(px(40.))
                                .rounded(px(99.))
                                .bg(rgb(swatch.text))
                                .opacity(0.38),
                        )
                        .child(
                            div()
                                .h(px(6.))
                                .w(px(28.))
                                .rounded(px(99.))
                                .bg(rgb(swatch.text))
                                .opacity(0.32),
                        )
                        .child(
                            div()
                                .h(px(6.))
                                .w(px(22.))
                                .rounded(px(99.))
                                .bg(rgb(swatch.text))
                                .opacity(0.26),
                        ),
                )
                .child(
                    div()
                        .mt(px(6.))
                        .h(px(3.))
                        .w(px(32.))
                        .rounded(px(99.))
                        .bg(rgb(swatch.accent)),
                ),
        )
}

fn theme_option(
    index: u32,
    swatch: &'static ThemeSwatch,
    selected: bool,
    zh: bool,
    chrome: Chrome,
    entity: Entity<SettingsWindow>,
) -> impl IntoElement {
    let id = swatch.id;
    let label = if zh { swatch.label_zh } else { swatch.label_en };
    div()
        .id(("ec-theme", index))
        .w(px(148.))
        .cursor_pointer()
        .child(
            div()
                .rounded(px(13.))
                .border_2()
                .border_color(rgb(if selected { chrome.accent } else { chrome.card }))
                .p(px(4.))
                .child(theme_preview_window(swatch)),
        )
        .child(
            div()
                .mt(px(6.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .w(px(14.))
                        .h(px(14.))
                        .rounded(px(99.))
                        .border_1()
                        .border_color(rgb(if selected { chrome.accent } else { chrome.separator }))
                        .bg(rgb(if selected { chrome.accent } else { chrome.card }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(if selected {
                            div()
                                .w(px(4.))
                                .h(px(4.))
                                .rounded(px(99.))
                                .bg(rgb(0xffffff))
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(chrome.text))
                        .child(label.to_string()),
                ),
        )
        .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
            entity.update(cx, |this, cx| this.set_string("autocomplete.theme", id, cx));
        })
}

fn theme_picker(zh: bool, chrome: Chrome, current: &str, entity: Entity<SettingsWindow>) -> impl IntoElement {
    let groups = [
        (ThemeAppearance::System, if zh { "自动" } else { "Automatic" }),
        (ThemeAppearance::Light, if zh { "浅色" } else { "Light" }),
        (ThemeAppearance::Dark, if zh { "深色" } else { "Dark" }),
    ];
    let mut root = div().flex().flex_col().gap(px(20.)).p(px(16.));
    for (appearance, label) in groups {
        let themes: Vec<(u32, &ThemeSwatch)> = THEMES
            .iter()
            .enumerate()
            .filter(|(_, swatch)| swatch.appearance == appearance)
            .map(|(i, swatch)| (i as u32, swatch))
            .collect();
        if themes.is_empty() {
            continue;
        }
        let mut grid = div().flex().flex_row().flex_wrap().gap(px(16.));
        for (index, swatch) in themes {
            grid = grid.child(theme_option(
                index,
                swatch,
                current == swatch.id,
                zh,
                chrome,
                entity.clone(),
            ));
        }
        root = root.child(
            div()
                .child(
                    div()
                        .mb(px(10.))
                        .text_size(px(11.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(chrome.muted))
                        .child(label.to_string()),
                )
                .child(grid),
        );
    }
    root
}

fn appearance_page(zh: bool, chrome: Chrome, entity: Entity<SettingsWindow>) -> impl IntoElement {
    let lang = fig_settings::settings::get_string_or("dashboard.language", "system".into());
    let theme = fig_settings::settings::get_string_or("autocomplete.theme", "github-dark".into());
    let font = fig_settings::settings::get_string_or("autocomplete.fontFamily", String::new());
    let font_size = fig_settings::settings::get_int_or("autocomplete.fontSize", 13);
    let width = fig_settings::settings::get_int_or("autocomplete.width", 300);
    let height = fig_settings::settings::get_int_or("autocomplete.height", 140);

    let lang_options: &[(&str, &str)] = if zh {
        &[("跟随系统", "system"), ("English", "en"), ("简体中文", "zh-CN")]
    } else {
        &[("Follow System", "system"), ("English", "en"), ("简体中文", "zh-CN")]
    };

    let theme_grid = theme_picker(zh, chrome, theme.as_str(), entity.clone());

    let mut font_options: Vec<(&str, String)> = FONTS.iter().map(|name| (*name, (*name).to_string())).collect();
    if !font.is_empty() && !FONTS.contains(&font.as_str()) {
        font_options.insert(0, ("Custom", font.clone()));
    }
    font_options.insert(0, (if zh { "系统默认" } else { "System default" }, String::new()));

    let mut font_row = div().flex().flex_row().flex_wrap().gap(px(6.));
    for (i, (label, value)) in font_options.iter().enumerate() {
        let selected = value == &font || (value.is_empty() && font.is_empty());
        let value = value.clone();
        let entity = entity.clone();
        font_row = font_row.child(
            div()
                .id(("ec-font", i as u32))
                .px(px(10.))
                .py(px(5.))
                .rounded(px(8.))
                .cursor_pointer()
                .bg(rgb(if selected { chrome.accent } else { chrome.separator }))
                .text_color(rgb(if selected { 0xffffff } else { chrome.text }))
                .child((*label).to_string())
                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                    let payload = if value.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(value.clone())
                    };
                    entity.update(cx, |this, cx| this.set_string("autocomplete.fontFamily", payload, cx));
                }),
        );
    }

    let lang_entity = entity.clone();
    let size_entity = entity.clone();
    let width_entity = entity.clone();
    let height_entity = entity.clone();

    div()
        .flex()
        .flex_col()
        .child(card(
            if zh { "语言" } else { "Language" },
            chrome,
            stacked_row(
                if zh { "显示语言" } else { "Display Language" },
                Some(if zh {
                    "选择设置面板使用的语言"
                } else {
                    "Choose the language used in the settings panel"
                }),
                chrome,
                true,
                select_chips("ec-lang", lang_options, lang.as_str(), chrome, move |value, cx| {
                    lang_entity.update(cx, |this, cx| this.set_string("dashboard.language", value, cx));
                }),
            ),
        ))
        .child(card(if zh { "主题" } else { "Theme" }, chrome, theme_grid))
        .child(card(
            if zh { "字体" } else { "Typography" },
            chrome,
            div()
                .child(stacked_row(
                    if zh { "字体名称" } else { "Font Family" },
                    Some(if zh {
                        "补全弹窗使用的字体"
                    } else {
                        "Font used in the autocomplete popup"
                    }),
                    chrome,
                    false,
                    font_row,
                ))
                .child(row(
                    if zh { "字体大小" } else { "Font Size" },
                    None,
                    chrome,
                    true,
                    stepper(
                        "font-size",
                        font_size.clamp(10, 24),
                        10,
                        24,
                        1,
                        chrome,
                        move |value, cx| {
                            size_entity.update(cx, |this, cx| this.set_int("autocomplete.fontSize", value, cx));
                        },
                    ),
                )),
        ))
        .child(card(
            if zh { "尺寸" } else { "Dimensions" },
            chrome,
            div()
                .child(row(
                    if zh { "最大宽度" } else { "Max Width" },
                    None,
                    chrome,
                    false,
                    stepper(
                        "max-width",
                        width.clamp(150, 800),
                        150,
                        800,
                        10,
                        chrome,
                        move |value, cx| {
                            width_entity.update(cx, |this, cx| this.set_int("autocomplete.width", value, cx));
                        },
                    ),
                ))
                .child(row(
                    if zh { "最大高度" } else { "Max Height" },
                    None,
                    chrome,
                    true,
                    stepper(
                        "max-height",
                        height.clamp(80, 600),
                        80,
                        600,
                        10,
                        chrome,
                        move |value, cx| {
                            height_entity.update(cx, |this, cx| this.set_int("autocomplete.height", value, cx));
                        },
                    ),
                )),
        ))
}

fn behavior_page(zh: bool, chrome: Chrome, entity: Entity<SettingsWindow>) -> impl IntoElement {
    let launch = fig_settings::settings::get_bool_or("app.launchOnStartup", false);
    let silent = fig_settings::settings::get_bool_or("app.silentLaunch", false);
    let only_tab = fig_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false);
    let fuzzy = fig_settings::settings::get_bool_or("autocomplete.fuzzySearch", true);
    let first_token = fig_settings::settings::get_bool_or("autocomplete.firstTokenCompletion", false);
    let sort = fig_settings::settings::get_string_or("autocomplete.sortMethod", "default".into());
    let history_nav = fig_settings::settings::get_bool_or("autocomplete.navigateToHistory", false);
    let trailing = fig_settings::settings::get_bool_or("autocomplete.insertSpaceAutomatically", true);
    let hide_auto = fig_settings::settings::get_bool_or("autocomplete.hideAutoExecuteSuggestion", false);
    let show_auto = !hide_auto;
    let exec_space = fig_settings::settings::get_bool_or("autocomplete.immediatelyExecuteAfterSpace", false);
    let dangerous = fig_settings::settings::get_bool_or("autocomplete.immediatelyRunDangerousCommands", false);
    let history_mode = fig_settings::settings::get_string_or("beta.history.mode", "show".into());
    let merge = fig_settings::settings::get_bool_or("beta.history.allShells", false);

    let e = |entity: &Entity<SettingsWindow>| entity.clone();

    div()
        .flex()
        .flex_col()
        .child(card(
            if zh { "启动与触发" } else { "Startup & Trigger" },
            chrome,
            div()
                .child(bool_row(
                    if zh { "登录时启动" } else { "Launch at Login" },
                    None,
                    launch,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| {
                        this.set_bool("app.launchOnStartup", value, cx);
                        tokio::spawn(async move {
                            if let Err(err) = fig_integrations::launch_at_login::set_enabled(value).await {
                                error!(%err, "Failed to update launch at login");
                            }
                        });
                    },
                ))
                .child(bool_row(
                    if zh { "静默启动" } else { "Silent Launch" },
                    Some(if zh {
                        "启动时不打开本设置窗口，直接在后台运行"
                    } else {
                        "Start in the background without opening this settings window"
                    }),
                    silent,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("app.silentLaunch", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "按 Tab 后显示建议"
                    } else {
                        "Show Suggestions After Tab"
                    },
                    None,
                    only_tab,
                    chrome,
                    true,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.onlyShowOnTab", value, cx),
                )),
        ))
        .child(card(
            if zh { "补全建议" } else { "Suggestions" },
            chrome,
            div()
                .child(bool_row(
                    if zh { "模糊匹配" } else { "Fuzzy Matching" },
                    Some(if zh {
                        "匹配相近字符序列，而非仅匹配前缀"
                    } else {
                        "Match close character sequences instead of exact prefixes"
                    }),
                    fuzzy,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.fuzzySearch", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "输入时提示命令名"
                    } else {
                        "Suggest Commands While Typing"
                    },
                    None,
                    first_token,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.firstTokenCompletion", value, cx),
                ))
                .child(stacked_row(
                    if zh { "排序方式" } else { "Sort Order" },
                    None,
                    chrome,
                    true,
                    {
                        let entity = e(&entity);
                        select_chips(
                            "ec-sort",
                            if zh {
                                &[("按相关性", "default"), ("按字母顺序", "alphabetical")]
                            } else {
                                &[("By Relevance", "default"), ("Alphabetical", "alphabetical")]
                            },
                            sort.as_str(),
                            chrome,
                            move |value, cx| {
                                entity.update(cx, |this, cx| this.set_string("autocomplete.sortMethod", value, cx));
                            },
                        )
                    },
                )),
        ))
        .child(card(
            if zh { "键盘与插入" } else { "Keyboard & Insertion" },
            chrome,
            div()
                .child(bool_row(
                    if zh {
                        "使用上方向键浏览历史"
                    } else {
                        "Use Up Arrow for History"
                    },
                    None,
                    history_nav,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.navigateToHistory", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "自动插入尾随空格"
                    } else {
                        "Insert Trailing Space"
                    },
                    None,
                    trailing,
                    chrome,
                    false,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.insertSpaceAutomatically", value, cx),
                ))
                .child(bool_row(
                    if zh {
                        "显示立即执行"
                    } else {
                        "Show Immediately Execute"
                    },
                    None,
                    show_auto,
                    chrome,
                    !show_auto,
                    e(&entity),
                    |this, value, cx| this.set_bool("autocomplete.hideAutoExecuteSuggestion", !value, cx),
                ))
                .when(show_auto, {
                    let entity = e(&entity);
                    let entity2 = e(&entity);
                    move |this| {
                        this.child(bool_row(
                            if zh {
                                "空格结尾时置顶"
                            } else {
                                "Pin After a Trailing Space"
                            },
                            None,
                            exec_space,
                            chrome,
                            false,
                            entity,
                            |this, value, cx| this.set_bool("autocomplete.immediatelyExecuteAfterSpace", value, cx),
                        ))
                        .child(bool_row(
                            if zh {
                                "包含危险命令"
                            } else {
                                "Include Dangerous Commands"
                            },
                            None,
                            dangerous,
                            chrome,
                            true,
                            entity2,
                            |this, value, cx| this.set_bool("autocomplete.immediatelyRunDangerousCommands", value, cx),
                        ))
                    }
                }),
        ))
        .child(card(
            if zh { "历史记录" } else { "History" },
            chrome,
            div()
                .child(stacked_row(
                    if zh { "历史记录模式" } else { "History Mode" },
                    None,
                    chrome,
                    false,
                    {
                        let entity = e(&entity);
                        select_chips(
                            "ec-history",
                            if zh {
                                &[
                                    ("与补全建议一起显示", "show"),
                                    ("仅显示历史记录", "history_only"),
                                    ("关闭", "off"),
                                ]
                            } else {
                                &[
                                    ("Show with completions", "show"),
                                    ("History only", "history_only"),
                                    ("Off", "off"),
                                ]
                            },
                            history_mode.as_str(),
                            chrome,
                            move |value, cx| {
                                entity.update(cx, |this, cx| this.set_string("beta.history.mode", value, cx));
                            },
                        )
                    },
                ))
                .child(bool_row(
                    if zh { "合并所有 Shell" } else { "Merge All Shells" },
                    Some(if zh {
                        "包含所有 Shell（bash、zsh、fish）的历史记录"
                    } else {
                        "Include history from all shells (bash, zsh, fish)"
                    }),
                    merge,
                    chrome,
                    true,
                    e(&entity),
                    |this, value, cx| this.set_bool("beta.history.allShells", value, cx),
                )),
        ))
}

fn bool_row(
    label: &str,
    description: Option<&str>,
    checked: bool,
    chrome: Chrome,
    last: bool,
    entity: Entity<SettingsWindow>,
    on_change: impl Fn(&mut SettingsWindow, bool, &mut Context<'_, SettingsWindow>) + 'static,
) -> impl IntoElement {
    let next = !checked;
    row(
        label,
        description,
        chrome,
        last,
        toggle(
            SharedString::from(format!("toggle-{label}")),
            checked,
            chrome,
            move |cx| {
                entity.update(cx, |this, cx| on_change(this, next, cx));
            },
        ),
    )
}

fn about_page(zh: bool, chrome: Chrome, entity: Entity<SettingsWindow>, copied_doctor: bool) -> impl IntoElement {
    let version = env!("CARGO_PKG_VERSION");
    let auto_updates = !fig_settings::settings::get_bool_or("app.disableAutoupdates", false);
    let telemetry = fig_settings::settings::get_bool_or("telemetry.enabled", false);
    let entity_copy = entity.clone();
    let entity_updates = entity.clone();
    let entity_auto = entity.clone();
    let entity_tel = entity.clone();

    div()
        .flex()
        .flex_col()
        .child(card(
            "Easy Complete",
            chrome,
            div()
                .px(px(16.))
                .py(px(16.))
                .child(
                    div()
                        .text_size(px(20.))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("Easy Complete"),
                )
                .child(div().mt(px(4.)).text_color(rgb(chrome.muted)).child(if zh {
                    "适用于 macOS 的终端自动补全".to_string()
                } else {
                    "Terminal autocomplete for macOS".to_string()
                }))
                .child(
                    div()
                        .mt(px(10.))
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .child(pill(
                            format!("{} {version}", if zh { "版本" } else { "Version" }),
                            chrome,
                            move |cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(format!("Easy Complete {version}")));
                                entity_copy.update(cx, |_, cx| cx.notify());
                            },
                        ))
                        .child(pill(
                            if zh {
                                "检查更新".to_string()
                            } else {
                                "Check for Updates".to_string()
                            },
                            chrome,
                            move |_cx| {
                                let _ = entity_updates;
                                tokio::spawn(async {
                                    crate::update::check_for_update(true, false).await;
                                });
                            },
                        )),
                ),
        ))
        .child(card(
            if zh { "更新" } else { "Updates" },
            chrome,
            bool_row(
                if zh {
                    "自动检查更新"
                } else {
                    "Check for Updates Automatically"
                },
                None,
                auto_updates,
                chrome,
                true,
                entity_auto,
                |this, value, cx| this.set_bool("app.disableAutoupdates", !value, cx),
            ),
        ))
        .child(card(
            if zh { "隐私" } else { "Privacy" },
            chrome,
            bool_row(
                if zh {
                    "分享匿名使用数据"
                } else {
                    "Share Anonymous Usage Data"
                },
                Some(if zh {
                    "仅匿名统计，从不包含命令或个人数据"
                } else {
                    "Anonymous statistics only, never commands or personal data"
                }),
                telemetry,
                chrome,
                true,
                entity_tel,
                |this, value, cx| this.set_bool("telemetry.enabled", value, cx),
            ),
        ))
        .child(card(
            if zh { "故障排查" } else { "Troubleshooting" },
            chrome,
            div()
                .px(px(16.))
                .py(px(14.))
                .child(div().child(if zh {
                    "在终端中运行内置诊断：".to_string()
                } else {
                    "Run the built-in diagnostic in your terminal:".to_string()
                }))
                .child({
                    let entity_cmd = entity.clone();
                    let entity_btn = entity.clone();
                    div()
                        .mt(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("ec-doctor-cmd")
                                .flex_1()
                                .px(px(12.))
                                .py(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(chrome.sidebar))
                                .font_family("Menlo")
                                .cursor_pointer()
                                .child("ec doctor")
                                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                    copy_doctor(&entity_cmd, cx);
                                }),
                        )
                        .child(
                            div()
                                .id("ec-doctor-copy")
                                .px(px(12.))
                                .py(px(8.))
                                .rounded(px(8.))
                                .bg(rgb(if copied_doctor { chrome.accent } else { chrome.separator }))
                                .text_color(rgb(if copied_doctor { 0xffffff } else { chrome.text }))
                                .cursor_pointer()
                                .child(if copied_doctor {
                                    if zh { "已复制" } else { "Copied" }.to_string()
                                } else if zh {
                                    "复制".to_string()
                                } else {
                                    "Copy".to_string()
                                })
                                .on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                    copy_doctor(&entity_btn, cx);
                                }),
                        )
                }),
        ))
        .child(card(
            if zh { "链接" } else { "Links" },
            chrome,
            div()
                .child(link_row(
                    if zh { "发行说明" } else { "Release Notes" },
                    fig_util::consts::url::RELEASE_NOTES,
                    chrome,
                    false,
                ))
                .child(link_row(
                    if zh { "报告问题" } else { "Report an Issue" },
                    fig_util::consts::url::ISSUE_TRACKER,
                    chrome,
                    false,
                ))
                .child(link_row(
                    if zh { "隐私政策" } else { "Privacy Policy" },
                    "https://easy-complete.emmmm.dev/privacy-policy",
                    chrome,
                    false,
                ))
                .child(link_row(
                    if zh { "开源许可证" } else { "Open Source Licenses" },
                    "file",
                    chrome,
                    true,
                )),
        ))
}

fn copy_doctor(entity: &Entity<SettingsWindow>, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string("ec doctor".into()));
    entity.update(cx, |this, cx| {
        this.copied_doctor = true;
        cx.notify();
    });
    let entity = entity.clone();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(2)).await;
        let _ = cx.update(|cx| {
            entity.update(cx, |this, cx| {
                this.copied_doctor = false;
                cx.notify();
            });
        });
    })
    .detach();
}

fn perm_label(id: PermId, zh: bool) -> (&'static str, &'static str, &'static str) {
    match (id, zh) {
        (PermId::Accessibility, true) => (
            "辅助功能权限",
            "用于读取当前聚焦的终端窗口并定位补全弹窗。点击后把 Easy Complete 拖进系统设置的列表。",
            "授予辅助功能权限",
        ),
        (PermId::Accessibility, false) => (
            "Accessibility Permission",
            "Required to read the focused terminal window and position completions. Drag Easy Complete into the system list after clicking.",
            "Grant Accessibility",
        ),
        (PermId::Shell, true) => (
            "Shell 集成",
            "向 .zshrc / .bashrc 注入钩子，使 Easy Complete 能够跟踪 Shell 状态。",
            "安装 Shell 钩子",
        ),
        (PermId::Shell, false) => (
            "Shell Integration",
            "Injects hooks into .zshrc / .bashrc so Easy Complete can track your shell state.",
            "Install Shell Hooks",
        ),
        (PermId::InputMethod, true) => (
            "输入法集成",
            "用于在 Kitty、Alacritty、Zed、Ghostty 和 WezTerm 中跟踪光标位置。",
            "安装输入法",
        ),
        (PermId::InputMethod, false) => (
            "Input Method Integration",
            "Required for cursor tracking in Kitty, Alacritty, Zed, Ghostty, and WezTerm.",
            "Install Input Method",
        ),
    }
}

fn perm_status_label(state: PermReady, zh: bool) -> &'static str {
    match (state, zh) {
        (PermReady::Checking, true) => "检查中",
        (PermReady::Checking, false) => "Checking",
        (PermReady::Ready, true) => "已就绪",
        (PermReady::Ready, false) => "Ready",
        (PermReady::Missing, true) => "需要设置",
        (PermReady::Missing, false) => "Needs setup",
        (PermReady::Error, true) => "需要处理",
        (PermReady::Error, false) => "Needs attention",
    }
}

fn permission_gate_page(
    zh: bool,
    chrome: Chrome,
    gate: PermissionSnapshot,
    repairing: Option<PermId>,
    entity: Entity<SettingsWindow>,
) -> impl IntoElement {
    #[cfg(target_os = "macos")]
    let rows = [
        (PermId::Accessibility, gate.accessibility),
        (PermId::Shell, gate.shell),
        (PermId::InputMethod, gate.input_method),
    ];
    #[cfg(not(target_os = "macos"))]
    let rows = [(PermId::Shell, gate.shell)];
    let checking = gate.still_checking();
    let busy = repairing.is_some() || checking;

    let mut list = div()
        .rounded(px(14.))
        .bg(rgb(chrome.card))
        .border_1()
        .border_color(rgb(chrome.separator));
    for (i, (id, state)) in rows.iter().enumerate() {
        let id = *id;
        let state = *state;
        let (title, description, repair_label) = perm_label(id, zh);
        let can_repair = matches!(state, PermReady::Missing | PermReady::Error);
        // macOS still sequences shell repair behind Accessibility. Linux/Windows
        // never ask for AX/IME, so a leftover Missing/Error bit must not hide
        // the only row the gate can actually repair.
        #[cfg(target_os = "macos")]
        let blocked = id == PermId::Shell && gate.accessibility != PermReady::Ready;
        #[cfg(not(target_os = "macos"))]
        let blocked = false;
        let this_busy = repairing == Some(id);
        let enabled = can_repair && !busy && !blocked;
        let last = i + 1 == rows.len();
        let entity_row = entity.clone();
        let mut row = div()
            .id(("ec-perm-row", i as u32))
            .px(px(18.))
            .py(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(16.));
        if !last {
            row = row.border_b_1().border_color(rgb(chrome.separator));
        }
        list = list.child(
            row.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_size(px(14.))
                                    .child(title.to_string()),
                            )
                            .child(
                                div()
                                    .px(px(8.))
                                    .py(px(3.))
                                    .rounded(px(999.))
                                    .bg(rgb(if state == PermReady::Ready { 0x1c3d2a } else { 0x3d2e16 }))
                                    .text_color(rgb(if state == PermReady::Ready { 0x30d158 } else { 0xff9f0a }))
                                    .text_size(px(12.))
                                    .child(perm_status_label(state, zh).to_string()),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(4.))
                            .text_size(px(12.))
                            .text_color(rgb(chrome.muted))
                            .child(description.to_string()),
                    )
                    .when(blocked, |this| {
                        this.child(
                            div()
                                .mt(px(4.))
                                .text_size(px(12.))
                                .text_color(rgb(chrome.muted))
                                .child(if zh {
                                    "请先授予辅助功能权限，再执行此步骤。"
                                } else {
                                    "Grant Accessibility first to enable this step."
                                }),
                        )
                    }),
            )
            .child(
                div()
                    .id(("ec-perm-repair", i as u32))
                    .min_w(px(130.))
                    .px(px(12.))
                    .py(px(6.))
                    .rounded(px(9.))
                    .bg(rgb(if enabled { chrome.accent } else { chrome.separator }))
                    .text_color(rgb(if enabled { 0xffffff } else { chrome.muted }))
                    .cursor_pointer()
                    .child(if this_busy {
                        if zh { "处理中…" } else { "Working..." }.to_string()
                    } else {
                        repair_label.to_string()
                    })
                    .when(enabled, |this| {
                        this.on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                            entity_row.update(cx, |this, cx| {
                                this.repairing = Some(id);
                                this.gate.error = None;
                                cx.notify();
                                permissions::spawn_repair(&this.proxy, id);
                            });
                        })
                    }),
            ),
        );
    }

    let entity_refresh = entity.clone();
    let entity_all = entity.clone();
    let entity_tel = entity.clone();
    let telemetry = fig_settings::settings::get_bool_or("telemetry.enabled", false);

    div()
        .id("ec-permission-gate")
        .flex()
        .flex_1()
        .flex_col()
        .items_center()
        .justify_center()
        .px(px(40.))
        .child(
            div()
                .w(px(640.))
                .child(
                    div()
                        .mb(px(16.))
                        .child(
                            div()
                                .text_size(px(22.))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(if zh { "完成设置" } else { "Finish Setup" }.to_string()),
                        )
                        .child(
                            div()
                                .mt(px(6.))
                                .text_size(px(13.))
                                .text_color(rgb(chrome.muted))
                                .child(if zh {
                                    "使用设置前，Easy Complete 需要以下权限。"
                                } else {
                                    "Easy Complete needs these permissions before settings can be used."
                                }),
                        ),
                )
                .child(list)
                .when_some(gate.error.clone(), |this, err| {
                    this.child(
                        div()
                            .mt(px(8.))
                            .text_size(px(12.))
                            .text_color(rgb(0xff453a))
                            .child(err),
                    )
                })
                .child(
                    div()
                        .mt(px(16.))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("ec-perm-refresh")
                                .px(px(12.))
                                .py(px(6.))
                                .rounded(px(9.))
                                .bg(rgb(chrome.separator))
                                .cursor_pointer()
                                .child(if checking {
                                    if zh { "检查中…" } else { "Checking..." }.to_string()
                                } else if zh {
                                    "重新检查".to_string()
                                } else {
                                    "Check Again".to_string()
                                })
                                .when(!busy, |this| {
                                    this.on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                        entity_refresh.update(cx, |this, cx| {
                                            this.gate = PermissionSnapshot::checking();
                                            cx.notify();
                                            permissions::spawn_check(&this.proxy);
                                        });
                                    })
                                }),
                        )
                        .child(
                            div()
                                .id("ec-perm-fix-all")
                                .px(px(12.))
                                .py(px(6.))
                                .rounded(px(9.))
                                .bg(rgb(if busy { chrome.separator } else { chrome.accent }))
                                .text_color(rgb(if busy { chrome.muted } else { 0xffffff }))
                                .cursor_pointer()
                                .child(if repairing.is_some() {
                                    if zh { "处理中…" } else { "Working..." }.to_string()
                                } else if zh {
                                    "全部修复".to_string()
                                } else {
                                    "Fix All".to_string()
                                })
                                .when(!busy, |this| {
                                    this.on_mouse_down(MouseButton::Left, move |_e, _w, cx| {
                                        entity_all.update(cx, |this, cx| {
                                            this.repairing = Some(PermId::Accessibility);
                                            cx.notify();
                                            permissions::spawn_repair_all(&this.proxy);
                                        });
                                    })
                                }),
                        ),
                )
                .child(
                    div()
                        .mt(px(20.))
                        .px(px(16.))
                        .py(px(12.))
                        .rounded(px(12.))
                        .bg(rgb(chrome.sidebar))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(16.))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .child(
                                    div()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(if zh {
                                            "共享匿名使用数据"
                                        } else {
                                            "Share anonymous usage data"
                                        }),
                                )
                                .child(
                                    div()
                                        .mt(px(3.))
                                        .text_size(px(12.))
                                        .text_color(rgb(chrome.muted))
                                        .child(if zh {
                                            "帮助我们了解安装数量和使用中的 macOS 版本。不会收集命令、路径或个人数据。"
                                        } else {
                                            "Helps us understand install counts and which macOS versions are in use. No commands, paths, or personal data are collected."
                                        }),
                                ),
                        )
                        .child(toggle(
                            "ec-perm-telemetry".into(),
                            telemetry,
                            chrome,
                            move |cx| {
                                entity_tel.update(cx, |this, cx| {
                                    this.set_bool("telemetry.enabled", !telemetry, cx);
                                });
                            },
                        )),
                ),
        )
}

fn pill(label: String, chrome: Chrome, on_click: impl Fn(&mut App) + 'static) -> impl IntoElement {
    div()
        .id(SharedString::from(label.clone()))
        .px(px(10.))
        .py(px(5.))
        .rounded(px(999.))
        .bg(rgb(chrome.separator))
        .cursor_pointer()
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_e, _w, cx| on_click(cx))
}

fn link_row(label: &str, url: &str, chrome: Chrome, last: bool) -> impl IntoElement {
    let url = url.to_string();
    row(
        label,
        None,
        chrome,
        last,
        div()
            .id(SharedString::from(format!("link-{label}")))
            .text_color(rgb(chrome.accent))
            .cursor_pointer()
            .child("↗")
            .on_mouse_down(MouseButton::Left, move |_e, _w, _cx| {
                if url == "file" {
                    if let Some(path) = notices_path() {
                        let _ = std::process::Command::new("open").arg(path).spawn();
                    }
                } else if let Err(err) = fig_util::open_url(&url) {
                    error!(%err, "Failed to open url");
                }
            }),
    )
}

fn notices_path() -> Option<PathBuf> {
    fig_util::directories::resources_path()
        .ok()
        .map(|dir| dir.join("Licenses/THIRD_PARTY_NOTICES.txt"))
}

fn locale_is_zh() -> bool {
    let pref = fig_settings::settings::get_string_or("dashboard.language", "system".into());
    match pref.as_str() {
        "zh-CN" | "zh" => true,
        "en" => false,
        _ => system_locale_is_zh(),
    }
}

fn system_locale_is_zh() -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, sel, sel_impl};
        let langs: id = msg_send![class!(NSLocale), preferredLanguages];
        if langs == nil {
            return false;
        }
        let first: id = msg_send![langs, firstObject];
        if first == nil {
            return false;
        }
        let prefix = NSString::alloc(nil).init_str("zh");
        let matched: bool = msg_send![first, hasPrefix: prefix];
        matched
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

fn activate_app() {
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::base::{YES, id};
        use objc::{class, msg_send, sel, sel_impl};
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, activateIgnoringOtherApps: YES];
    }
}

pub fn open_settings_window(cx: &mut App, proxy: EventLoopProxy) -> anyhow::Result<SettingsHandle> {
    let bounds = Bounds::centered(None, size(px(WIN_W), px(WIN_H)), cx);
    let close_proxy = proxy.clone();
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SETTINGS_WINDOW_TITLE.into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.), px(18.))),
            }),
            focus: true,
            show: true,
            kind: gpui::WindowKind::Normal,
            is_movable: true,
            is_resizable: true,
            window_min_size: Some(size(px(WIN_W), px(520.))),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(SETTINGS_WINDOW_TITLE);
            window.on_window_should_close(cx, move |_window, _cx| {
                close_proxy
                    .send_event(Event::WindowEvent {
                        window_id: DASHBOARD_ID,
                        window_event: WindowEvent::Close,
                    })
                    .ok();
                true
            });
            cx.new(|_| SettingsWindow {
                section: Section::Appearance,
                proxy: proxy.clone(),
                gate: PermissionSnapshot::checking(),
                repairing: None,
                copied_doctor: false,
            })
        },
    )?;
    handle
        .update(cx, |_view, window, _cx| {
            window.activate_window();
        })
        .ok();
    activate_app();
    start_permission_poller(handle, cx);
    Ok(handle)
}

fn start_permission_poller(handle: SettingsHandle, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let keep_going = match cx.update(|cx| {
                handle.update(cx, |this, _window, _cx| {
                    if this.gate.all_ready() {
                        return false;
                    }
                    let ax_now_ready = !permissions::accessibility_is_missing();
                    let ax_marked_ready = this.gate.accessibility == PermReady::Ready;
                    if ax_now_ready != ax_marked_ready {
                        this.proxy.send_event(Event::ReloadAccessibility).ok();
                    }
                    true
                })
            }) {
                Ok(Ok(keep)) => keep,
                _ => false,
            };
            if !keep_going {
                break;
            }
        }
    })
    .detach();
}

pub fn apply_permission_snapshot(handle: &SettingsHandle, snapshot: PermissionSnapshot, cx: &mut App) {
    handle
        .update(cx, |view, _window, cx| {
            view.gate = snapshot;
            view.repairing = None;
            cx.notify();
        })
        .ok();
}

pub fn focus_settings(handle: &SettingsHandle, cx: &mut App) -> bool {
    let ok = handle
        .update(cx, |_view, window, _cx| {
            window.activate_window();
        })
        .is_ok();
    if ok {
        activate_app();
    }
    ok
}

pub fn close_settings(handle: &SettingsHandle, cx: &mut App) {
    handle
        .update(cx, |_view, window, _cx| {
            window.remove_window();
        })
        .ok();
}

pub fn set_settings_section(handle: &SettingsHandle, path: &str, cx: &mut App) {
    let section = if path.contains("behavior") {
        Section::Behavior
    } else if path.contains("about") {
        Section::About
    } else {
        Section::Appearance
    };
    handle
        .update(cx, |view, _window, cx| {
            view.section = section;
            cx.notify();
        })
        .ok();
}

pub fn notify_dashboard_visible(proxy: &EventLoopProxy, visible: bool) {
    SETTINGS_OPEN.store(visible, Ordering::Relaxed);
    proxy
        .send_event(Event::PlatformBoundEvent(PlatformBoundEvent::FullscreenStateUpdated {
            fullscreen: false,
            dashboard_visible: Some(visible),
        }))
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermReady;

    fn snapshot(accessibility: PermReady, shell: PermReady, input_method: PermReady) -> PermissionSnapshot {
        PermissionSnapshot {
            accessibility,
            shell,
            input_method,
            error: None,
        }
    }

    #[test]
    fn settings_do_not_show_the_gate_while_permissions_are_checking() {
        assert!(!shows_permission_gate(&PermissionSnapshot::checking()));
        assert!(!shows_permission_gate(&snapshot(
            PermReady::Ready,
            PermReady::Checking,
            PermReady::Ready
        )));
        #[cfg(not(target_os = "macos"))]
        assert!(
            !shows_permission_gate(&snapshot(PermReady::Checking, PermReady::Ready, PermReady::Checking)),
            "Linux/Windows must not wait on macOS Accessibility/IME"
        );
    }

    #[test]
    fn settings_show_the_gate_only_after_a_failed_check() {
        assert!(!shows_permission_gate(&snapshot(
            PermReady::Ready,
            PermReady::Ready,
            PermReady::Ready
        )));
        #[cfg(target_os = "macos")]
        assert!(shows_permission_gate(&snapshot(
            PermReady::Missing,
            PermReady::Ready,
            PermReady::Ready
        )));
        #[cfg(not(target_os = "macos"))]
        {
            assert!(
                !shows_permission_gate(&snapshot(PermReady::Missing, PermReady::Ready, PermReady::Ready)),
                "Linux/Windows must not trap settings behind macOS Accessibility"
            );
            assert!(
                !shows_permission_gate(&snapshot(PermReady::Error, PermReady::Ready, PermReady::Missing)),
                "Linux/Windows must not trap settings behind macOS Accessibility/IME"
            );
            assert!(shows_permission_gate(&snapshot(
                PermReady::Ready,
                PermReady::Missing,
                PermReady::Ready
            )));
        }
    }

    #[test]
    fn theme_catalog_keeps_the_old_groups() {
        let ids = |appearance: ThemeAppearance| -> Vec<&'static str> {
            THEMES
                .iter()
                .filter(|theme| theme.appearance == appearance)
                .map(|theme| theme.id)
                .collect()
        };
        assert_eq!(ids(ThemeAppearance::System), ["system"]);
        assert_eq!(
            ids(ThemeAppearance::Light),
            ["light", "github-light", "claude-light", "catppuccin-latte"]
        );
        assert_eq!(
            ids(ThemeAppearance::Dark),
            [
                "dark",
                "github-dark",
                "claude-dark",
                "nord",
                "gruvbox-dark",
                "one-dark",
                "tokyo-night"
            ]
        );
    }

    /// A swatch whose id has no theme file falls back to [`OverlayTheme::dark`]
    /// without a word (`load_named_theme` swallows the miss), so the picker would
    /// silently offer the wrong colors.
    #[test]
    fn every_offered_theme_has_a_file_or_is_built_in() {
        for theme in THEMES {
            if matches!(theme.id, "system" | "light" | "dark") {
                continue;
            }
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../themes")
                .join(format!("{}.json", theme.id));
            assert!(
                path.exists(),
                "{} is offered but themes/{}.json is missing",
                theme.id,
                theme.id
            );
        }
    }
}
