//! Standalone GPUI overlay lab binary. Not shipped. Not in `default-members`.
//!
//! Run with:
//! `cargo run -p ec_overlay_spike --bin ec-overlay-spike`
//!
//! The window is a transparent, non-activating `PopUp` that stays above other
//! apps. Click a row or use Up/Down; Esc hides. This does not talk to figterm.
//! Dist profiles (`build-app.sh`, `build-linux.sh`) do not build this crate.

use ec_gpui::{
    OVERLAY_WINDOW_TITLE, OverlayState, SuggestionItem, open_overlay_window_with_visibility, set_overlay_visible_titled,
};
use gpui::{
    App, AppContext as _, Application, KeyBinding, Menu, MenuItem, WindowOptions, actions, div, prelude::*, px, rgb,
    size,
};

actions!(spike, [Quit, MoveUp, MoveDown, HideOverlay]);

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("escape", HideOverlay, None),
            KeyBinding::new("up", MoveUp, None),
            KeyBinding::new("down", MoveDown, None),
        ]);
        cx.set_menus(vec![Menu {
            name: "Easy Complete Overlay".into(),
            items: vec![MenuItem::action("Quit", Quit)],
        }]);

        let state = cx.new(|_| {
            let mut overlay = OverlayState::new();
            overlay.set_suggestions(
                vec![
                    SuggestionItem {
                        name: "checkout".into(),
                        description: "Switch branches or restore working tree files".into(),
                        kind: "subcommand".into(),
                        ..SuggestionItem::default()
                    },
                    SuggestionItem {
                        name: "commit".into(),
                        description: "Record changes to the repository".into(),
                        kind: "subcommand".into(),
                        ..SuggestionItem::default()
                    },
                    SuggestionItem {
                        name: "cherry-pick".into(),
                        description: "Apply the changes introduced by some existing commits".into(),
                        kind: "subcommand".into(),
                        ..SuggestionItem::default()
                    },
                    SuggestionItem {
                        name: "clone".into(),
                        description: "Clone a repository into a new directory".into(),
                        kind: "subcommand".into(),
                        ..SuggestionItem::default()
                    },
                ],
                "ch".into(),
            );
            overlay
        });

        open_overlay_window_with_visibility(cx, state.clone(), true).expect("open overlay");
        set_overlay_visible_titled(OVERLAY_WINDOW_TITLE, true);
        ec_gpui::harden_overlay_window_titled(OVERLAY_WINDOW_TITLE);

        // A tiny companion window so the spike has a focused app surface for keybindings.
        let bounds = gpui::Bounds::centered(None, size(px(280.), px(72.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Easy Complete Overlay Spike".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| SpikeHud),
        )
        .ok();

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action({
            let state = state.clone();
            move |_: &HideOverlay, cx| {
                state.update(cx, |overlay, cx| {
                    overlay.hide();
                    cx.notify();
                });
                set_overlay_visible_titled(OVERLAY_WINDOW_TITLE, false);
            }
        });
        cx.on_action({
            let state = state.clone();
            move |_: &MoveUp, cx| {
                state.update(cx, |overlay, cx| {
                    overlay.move_selection(-1);
                    cx.notify();
                });
            }
        });
        cx.on_action({
            let state = state.clone();
            move |_: &MoveDown, cx| {
                state.update(cx, |overlay, cx| {
                    overlay.move_selection(1);
                    cx.notify();
                });
            }
        });
        cx.activate(true);
    });
}

struct SpikeHud;

impl Render for SpikeHud {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut gpui::Context<'_, Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x1c1c1e))
            .text_color(rgb(0xf5f5f7))
            .size_full()
            .child("Overlay spike — Up/Down, Esc, Cmd-Q")
    }
}
