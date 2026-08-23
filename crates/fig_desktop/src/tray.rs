use std::borrow::Cow;
use std::sync::OnceLock;

use cfg_if::cfg_if;
use fig_install::InstallComponents;
use fig_os_shim::Context;
use fig_remote_ipc::figterm::FigtermState;
use fig_util::consts::PRODUCT_NAME;
use fig_util::url::USER_MANUAL;
use muda::accelerator::Accelerator;
use muda::{IconMenuItem, Menu, MenuEvent, MenuId, PredefinedMenuItem, Submenu};
use tracing::{error, trace, warn};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::event::{Event, ShowMessageNotification, WindowEvent};
use crate::{AUTOCOMPLETE_ID, EventLoopProxy, EventLoopWindowTarget, SETTINGS_ID};

// macro_rules! icon {
//     ($icon:literal) => {{
//         #[cfg(target_os = "macos")]
//         {
//             Some(include_bytes!(concat!(
//                 env!("TRAY_ICONS_PROCESSED"),
//                 "/",
//                 $icon,
//                 ".png"
//             )))
//         }
//         #[cfg(not(target_os = "macos"))]
//         {
//             None
//         }
//     }};
// }

const ACCESSIBILITY_MENU_ID: &str = "accessibility";

/// Autocomplete is fully inert without Accessibility, so the tray is the one surface that can say
/// so no matter how the app was launched — a silent launch never opens settings, where the
/// permission gate lives.
#[cfg(target_os = "macos")]
fn accessibility_is_missing() -> bool {
    !macos_utils::accessibility::accessibility_is_enabled()
}

#[cfg(not(target_os = "macos"))]
fn accessibility_is_missing() -> bool {
    false
}

fn tray_update(proxy: &EventLoopProxy) {
    let proxy = proxy.clone();
    tokio::runtime::Handle::current().spawn(async move {
        if !crate::update::check_for_update(true, true).await {
            proxy.send_event_or_warn(
                ShowMessageNotification {
                    title: format!("{PRODUCT_NAME} updates are unavailable").into(),
                    body: "The Sparkle updater could not start — the framework may be missing from this build or failed to initialize. Check the logs for details.".into(),
                    ..Default::default()
                }
                .into(),
            );
        }
    });
}

pub fn handle_event(menu_event: &MenuEvent, proxy: &EventLoopProxy) {
    match &*menu_event.id().0 {
        "autocomplete-devtools" => {
            proxy.send_event_or_warn(Event::WindowEvent {
                window_id: AUTOCOMPLETE_ID,
                window_event: WindowEvent::Devtools,
            });
        },
        "update" => {
            tray_update(proxy);
        },
        "quit" => {
            proxy.send_event_or_warn(Event::Quit);
        },
        "settings" => {
            proxy.send_event_or_warn(Event::WindowEvent {
                window_id: SETTINGS_ID.clone(),
                window_event: WindowEvent::Batch(vec![
                    WindowEvent::NavigateRelative {
                        path: "appearance".into(),
                    },
                    WindowEvent::Show,
                ]),
            });
        },
        "not-working" => {
            proxy.send_event_or_warn(Event::WindowEvent {
                window_id: SETTINGS_ID.clone(),
                window_event: WindowEvent::Batch(vec![
                    WindowEvent::NavigateRelative { path: "about".into() },
                    WindowEvent::Show,
                ]),
            });
        },
        "uninstall" => {
            tokio::runtime::Handle::current().spawn(async {
                fig_install::uninstall(InstallComponents::all(), Context::new())
                    .await
                    .ok();
                #[allow(clippy::exit)]
                std::process::exit(0);
            });
        },
        ACCESSIBILITY_MENU_ID => {
            #[cfg(target_os = "macos")]
            {
                use macos_utils::accessibility::{open_accessibility, prompt_for_accessibility};

                // `prompt_for_accessibility` only raises the system dialog while macOS still
                // considers the app un-prompted; once the bundle is listed — even with a stale
                // entry that no longer grants anything — it returns silently. Always open the
                // settings pane too so the user has somewhere to go in that case.
                prompt_for_accessibility();
                open_accessibility();
            }
        },
        "user-manual" => {
            if let Err(err) = fig_util::open_url(USER_MANUAL) {
                error!(%err, "Failed to open user manual url");
            }
        },
        id => {
            trace!(?id, "Unhandled tray event");
        },
    }

    // fig_telemetry removed
}

#[allow(dead_code)]
#[cfg(target_os = "linux")]
fn load_icon(path: impl AsRef<std::path::Path>) -> Option<Icon> {
    let image = image::open(path).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    let rgba = image.into_raw();
    Icon::from_rgba(rgba, width, height).ok()
}

#[allow(dead_code)] // used by AppRuntime on macOS/Windows; Linux uses linux_tray
pub async fn build_tray(
    _event_loop_window_target: &EventLoopWindowTarget,
    _figterm_state: &FigtermState,
) -> tray_icon::Result<TrayIcon> {
    build_tray_icon()
}

pub(crate) fn build_tray_icon() -> tray_icon::Result<TrayIcon> {
    let mut builder = TrayIconBuilder::new()
        .with_icon_as_template(true)
        .with_menu(Box::new(get_context_menu()));
    if let Some(icon) = get_icon() {
        builder = builder.with_icon(icon);
    } else {
        warn!("bundled tray icon missing or invalid; building tray without an icon");
    }
    builder.build()
}

/// Decode a bundled PNG into a tray icon. Invalid bytes warn and return `None`
/// instead of panicking the desktop process.
pub(crate) fn decode_tray_icon(bytes: &[u8]) -> Option<Icon> {
    let image = match image::load_from_memory(bytes) {
        Ok(image) => image.into_rgba8(),
        Err(err) => {
            warn!(?err, "failed to decode tray icon");
            return None;
        },
    };
    let (width, height) = image.dimensions();
    match Icon::from_rgba(image.into_raw(), width, height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            warn!(?err, "failed to build tray icon");
            None
        },
    }
}

pub fn get_icon() -> Option<Icon> {
    static ICON: OnceLock<Option<Icon>> = OnceLock::new();
    ICON.get_or_init(|| {
        cfg_if! {
            if #[cfg(target_os = "linux")] {
                decode_tray_icon(include_bytes!("../icons/icon-monochrome-light.png"))
            } else {
                decode_tray_icon(include_bytes!("../icons/icon-monochrome.png"))
            }
        }
    })
    .clone()
}

fn warning_icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
    static RGBA: OnceLock<Option<(Vec<u8>, u32, u32)>> = OnceLock::new();
    RGBA.get_or_init(|| {
        let image = match image::load_from_memory(include_bytes!("../icons/yellow-circle.png")) {
            Ok(image) => image.into_rgba8(),
            Err(err) => {
                warn!(?err, "failed to decode tray warning icon");
                return None;
            },
        };
        let (width, height) = image.dimensions();
        Some((image.into_raw(), width, height))
    })
    .clone()
}

pub fn get_context_menu() -> Menu {
    let mut tray_menu = Menu::new();

    for elem in menu() {
        elem.add_to_menu(&mut tray_menu);
    }

    tray_menu
}

enum MenuElement {
    Info {
        image_icon: Option<muda::Icon>,
        text: Cow<'static, str>,
    },
    Entry {
        emoji_icon: Option<Cow<'static, str>>,
        image_icon: Option<muda::Icon>,
        text: Cow<'static, str>,
        id: Cow<'static, str>,
        accelerator: Option<Accelerator>,
    },
    Separator,
    #[allow(dead_code)]
    SubMenu {
        title: Cow<'static, str>,
        elements: Vec<MenuElement>,
    },
}

impl MenuElement {
    fn info(image_icon: Option<(Vec<u8>, u32, u32)>, text: impl Into<Cow<'static, str>>) -> Self {
        Self::Info {
            image_icon: image_icon.and_then(|(bytes, width, height)| muda::Icon::from_rgba(bytes, width, height).ok()),
            text: text.into(),
        }
    }

    fn entry(
        emoji_icon: Option<Cow<'static, str>>,
        image_icon: Option<(Vec<u8>, u32, u32)>,
        text: impl Into<Cow<'static, str>>,
        id: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::Entry {
            emoji_icon,
            image_icon: image_icon.and_then(|(bytes, width, height)| muda::Icon::from_rgba(bytes, width, height).ok()),
            text: text.into(),
            id: id.into(),
            accelerator: None,
        }
    }

    fn with_accelerator(mut self, accel: &str) -> Self {
        if let Self::Entry {
            ref mut accelerator, ..
        } = self
        {
            *accelerator = accel.parse::<Accelerator>().ok();
        }
        self
    }

    // fn sub_menu(title: impl Into<Cow<'static, str>>, elements: Vec<MenuElement>) -> Self {
    //     Self::SubMenu {
    //         title: title.into(),
    //         elements,
    //     }
    // }

    fn add_to_menu(&self, menu: &mut Menu) {
        match self {
            MenuElement::Info { text, image_icon } => {
                let menu_item = IconMenuItem::new(
                    text,
                    false,
                    image_icon.clone(), // Some(muda::Icon::from_rgba(bytes, width, height).unwrap()),
                    None,
                );
                append_or_warn(menu.append(&menu_item));
            },
            MenuElement::Entry {
                emoji_icon,
                image_icon,
                text,
                id,
                accelerator,
            } => {
                let text = match (std::env::consts::OS, emoji_icon) {
                    ("linux", Some(emoji_icon)) => format!("{emoji_icon} {text}"),
                    _ => text.to_string(),
                };
                let menu_item = IconMenuItem::with_id(MenuId::new(id), text, true, image_icon.clone(), *accelerator);
                append_or_warn(menu.append(&menu_item));
            },
            MenuElement::Separator => {
                append_or_warn(menu.append(&PredefinedMenuItem::separator()));
            },
            MenuElement::SubMenu { title, elements } => {
                let sub_menu = Submenu::new(title, true);
                for element in elements {
                    element.add_to_submenu(&sub_menu);
                }

                append_or_warn(menu.append(&sub_menu));
            },
        }
    }

    fn add_to_submenu(&self, submenu: &Submenu) {
        match self {
            MenuElement::Info { image_icon, text } => {
                // menu.append(MenuItemAttributes::new(info).with_enabled(false));
                let menu_item = IconMenuItem::new(
                    text,
                    false,
                    image_icon.clone(), // Some(muda::Icon::from_rgba(bytes, width, height).unwrap()),
                    None,
                );
                append_or_warn(submenu.append(&menu_item));
            },
            MenuElement::Entry {
                emoji_icon,
                text,
                id,
                accelerator,
                ..
            } => {
                let text: String = match (std::env::consts::OS, emoji_icon) {
                    ("linux", Some(emoji_icon)) => format!("{emoji_icon} {text}"),
                    _ => text.to_string(),
                };
                let menu_item = muda::MenuItem::with_id(MenuId::new(id), text, true, *accelerator);
                append_or_warn(submenu.append(&menu_item));
            },
            MenuElement::Separator => {
                append_or_warn(submenu.append(&PredefinedMenuItem::separator()));
            },
            MenuElement::SubMenu { title, elements } => {
                let sub_menu = Submenu::new(title, true);
                for element in elements {
                    element.add_to_submenu(&sub_menu);
                }

                append_or_warn(submenu.append(&sub_menu));
            },
        }
    }
}

fn append_or_warn(result: muda::Result<()>) {
    if let Err(err) = result {
        warn!(%err, "failed to append tray menu item");
    }
}

fn menu() -> Vec<MenuElement> {
    let quit = MenuElement::entry(None, None, "Quit", "quit").with_accelerator("super+KeyQ");
    let settings = MenuElement::entry(None, None, "Settings", "settings").with_accelerator("super+Comma");
    let check_for_updates = MenuElement::entry(None, None, "Check for Updates…", "update");

    let mut menu = vec![settings, check_for_updates];

    if accessibility_is_missing() {
        let warning_img = warning_icon_rgba();
        let mut warning = vec![
            MenuElement::info(warning_img, "Accessibility permission is missing"),
            MenuElement::entry(None, None, "Enable Accessibility…", ACCESSIBILITY_MENU_ID),
            MenuElement::Separator,
        ];
        warning.append(&mut menu);
        menu = warning;
    }

    menu.extend(vec![MenuElement::Separator, quit]);

    menu
}

#[cfg(test)]
mod tests {
    #[test]
    fn tray_icon_decode_is_cached() {
        let first = super::get_icon();
        let second = super::get_icon();
        assert!(first.is_some(), "bundled tray icon must decode");
        assert!(second.is_some(), "bundled tray icon must decode");
        let warning = super::warning_icon_rgba().expect("yellow-circle.png must decode on the happy path");
        assert!(!warning.0.is_empty());
        assert_eq!(warning.0.len(), (warning.1 * warning.2 * 4) as usize);
    }

    #[test]
    fn invalid_tray_icon_bytes_do_not_panic() {
        assert!(super::decode_tray_icon(b"not-a-png").is_none());
        assert!(super::decode_tray_icon(&[]).is_none());
        // Truncated PNG header: enough to look like an image, not enough to decode.
        assert!(super::decode_tray_icon(b"\x89PNG\r\n\x1a\n").is_none());
    }

    #[test]
    fn get_icon_does_not_expect_on_decode() {
        let src = include_str!("tray.rs");
        // Concat so this pin's own source does not contain the old expect literals.
        assert!(
            !src.contains(&["expect(\"", "Failed to open icon path\")"].concat())
                && !src.contains(&["expect(\"", "Failed to open icon\")"].concat()),
            "get_icon / decode_tray_icon must not panic on a bad asset"
        );
        assert!(
            src.contains("decode_tray_icon") && src.contains("failed to decode tray icon"),
            "tray icon decode should warn and return None"
        );
    }

    #[test]
    fn tray_menu_append_does_not_unwrap() {
        let _menu = super::get_context_menu();
        let production = include_str!("tray.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            !production.contains("menu.append(") || !production.contains(&["append(&menu_item)", ".unwrap()"].concat()),
            "tray menu append must not panic the desktop"
        );
        assert!(
            !production.contains(&["append(&menu_item)", ".unwrap()"].concat())
                && !production.contains(&["separator())", ".unwrap()"].concat())
                && !production.contains(&["append(&sub_menu)", ".unwrap()"].concat()),
            "muda append failures warn and skip"
        );
        assert!(
            production.contains("append_or_warn") && production.contains("failed to append tray menu item"),
            "append errors must warn"
        );
    }

    #[test]
    fn tray_menu_is_not_an_auth_gate() {
        let production = include_str!("tray.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            !production.contains("Your session has expired")
                && !production.contains("Log back in")
                && !production.contains("hasn't been set up yet")
                && !production.contains("is_logged_in"),
            "fig_auth is gone; the tray must not offer a signed-out / session-expired menu"
        );
        assert!(
            production.contains("Settings") && production.contains("Check for Updates"),
            "signed-in tray entries still exist"
        );
        assert!(
            !production.contains("fn get_icon(is_logged_in") && production.contains("fn get_icon()"),
            "tray icon must not branch on a signed-in flag"
        );
    }
}
