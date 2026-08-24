#[allow(unused_imports)]
use fig_util::consts::PRODUCT_NAME;
use fig_util::consts::url::{ISSUE_TRACKER, RELEASE_NOTES, USER_MANUAL};
#[allow(unused_imports)]
use muda::{Menu, MenuEvent, Submenu};

use crate::event::{Event, WindowEvent};
use crate::{EventLoopProxy, SETTINGS_ID};

#[cfg(target_os = "macos")]
fn append_or_warn(result: muda::Result<()>) {
    if let Err(err) = result {
        tracing::warn!(%err, "failed to append menu bar item");
    }
}

const SETTINGS_QUIT: &str = "settings-quit";
const SETTINGS_CLOSE: &str = "settings-close";
const SETTINGS_ABOUT: &str = "settings-about";
const SETTINGS_CHECK_FOR_UPDATES: &str = "settings-check-for-updates";
const SETTINGS_OPEN_GITHUB: &str = "settings-open-github";
const SETTINGS_OPEN_RELEASE_NOTES: &str = "settings-open-release-notes";
const SETTINGS_REPORT_ISSUE: &str = "settings-report-issue";

#[cfg(target_os = "macos")]
pub fn menu_bar() -> Menu {
    use muda::{MenuItemBuilder, PredefinedMenuItem, Submenu};

    let menu_bar = Menu::new();

    let app_submenu = Submenu::new(PRODUCT_NAME, true);
    append_or_warn(
        app_submenu.append_items(&[
            &MenuItemBuilder::new()
                .text(format!("About {PRODUCT_NAME}"))
                .id(SETTINGS_ABOUT.into())
                .enabled(true)
                .build(),
            &MenuItemBuilder::new()
                .text("Check for Updates…")
                .id(SETTINGS_CHECK_FOR_UPDATES.into())
                .enabled(true)
                .build(),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(Some("Quit Easy Complete")),
        ]),
    );

    append_or_warn(menu_bar.append(&app_submenu));

    let file_submenu = Submenu::new("File", true);
    let close_window = {
        let builder = MenuItemBuilder::new()
            .text("Close Window")
            .id(SETTINGS_CLOSE.into())
            .enabled(true);
        match builder.accelerator(Some("super+w")) {
            Ok(builder) => builder.build(),
            Err(err) => {
                tracing::warn!(%err, "failed to set Close Window accelerator");
                MenuItemBuilder::new()
                    .text("Close Window")
                    .id(SETTINGS_CLOSE.into())
                    .enabled(true)
                    .build()
            },
        }
    };
    append_or_warn(file_submenu.append_items(&[&close_window]));

    append_or_warn(menu_bar.append(&file_submenu));

    let edit_submenu = Submenu::new("Edit", true);
    append_or_warn(edit_submenu.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::select_all(None),
    ]));

    append_or_warn(menu_bar.append(&edit_submenu));

    let window_submenu = Submenu::new("Window", true);
    append_or_warn(window_submenu.append_items(&[
        &PredefinedMenuItem::minimize(None),
        &PredefinedMenuItem::maximize(Some("Zoom")),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::bring_all_to_front(None),
    ]));

    append_or_warn(menu_bar.append(&window_submenu));

    let help_submenu = Submenu::new("Help", true);
    append_or_warn(
        help_submenu.append_items(&[
            &MenuItemBuilder::new()
                .text(format!("{PRODUCT_NAME} on GitHub"))
                .id(SETTINGS_OPEN_GITHUB.into())
                .enabled(true)
                .build(),
            &MenuItemBuilder::new()
                .text("Release Notes")
                .id(SETTINGS_OPEN_RELEASE_NOTES.into())
                .enabled(true)
                .build(),
            &MenuItemBuilder::new()
                .text("Report an Issue")
                .id(SETTINGS_REPORT_ISSUE.into())
                .enabled(true)
                .build(),
        ]),
    );

    append_or_warn(menu_bar.append(&help_submenu));

    menu_bar
}

#[cfg(not(target_os = "macos"))]
pub fn menu_bar() -> Menu {
    Menu::new()
}

pub fn handle_event(menu_event: &MenuEvent, proxy: &EventLoopProxy) {
    match &menu_event.id().0 {
        menu_id if menu_id == SETTINGS_QUIT => proxy.send_event_or_warn(Event::Quit),
        menu_id if menu_id == SETTINGS_CLOSE => proxy.send_event_or_warn(Event::WindowEvent {
            window_id: SETTINGS_ID,
            window_event: WindowEvent::Close,
        }),
        menu_id if menu_id == SETTINGS_ABOUT => proxy.send_event_or_warn(Event::WindowEvent {
            window_id: SETTINGS_ID,
            window_event: WindowEvent::Batch(vec![
                WindowEvent::NavigateRelative { path: "about".into() },
                WindowEvent::Show,
            ]),
        }),
        menu_id if menu_id == SETTINGS_CHECK_FOR_UPDATES => {
            tokio::runtime::Handle::current().spawn(async move {
                let _ = crate::update::check_for_update(true, true).await;
            });
        },
        menu_id if menu_id == SETTINGS_OPEN_GITHUB => {
            if let Err(err) = fig_util::open_url(USER_MANUAL) {
                tracing::error!(%err, "Failed to open project url");
            }
        },
        menu_id if menu_id == SETTINGS_OPEN_RELEASE_NOTES => {
            if let Err(err) = fig_util::open_url(RELEASE_NOTES) {
                tracing::error!(%err, "Failed to open release notes url");
            }
        },
        menu_id if menu_id == SETTINGS_REPORT_ISSUE => {
            if let Err(err) = fig_util::open_url(ISSUE_TRACKER) {
                tracing::error!(%err, "Failed to open issue tracker url");
            }
        },
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    // `muda::Menu` is main-thread-only on macOS and the test harness runs on a
    // worker, so this pins the source instead of building a live menu.
    #[test]
    fn menu_bar_append_does_not_unwrap() {
        let src = include_str!("menu.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        let macos = src.find("pub fn menu_bar()").expect("menu_bar");
        let rest = &src[macos..];
        let end = rest.find("#[cfg(not(target_os = \"macos\"))]").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains(".unwrap()"),
            "macOS menu bar append / accelerator must not panic the desktop"
        );
        assert!(
            body.contains("append_or_warn") && body.contains("failed to set Close Window accelerator"),
            "append and accelerator failures warn and skip"
        );
    }
}
