use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tao::dpi::{Position, Size};
#[cfg(any(target_os = "linux", test))]
use tao::window::Icon;
use tracing::warn;

/// Recover a [`Mutex`] after another thread panicked while holding it.
///
/// A poisoned lock still has a usable value; panicking the desktop over that
/// would drop the overlay and tray. Match overlay / figterm: `into_inner`.
pub(crate) fn recover_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| {
        warn!("recovered from poisoned mutex");
        err.into_inner()
    })
}

/// Determines if the build is ran in debug mode
#[allow(dead_code)]
pub fn is_cargo_debug_build() -> bool {
    cfg!(debug_assertions) && !fig_settings::state::get_bool_or("developer.override-cargo-debug", false)
}

#[cfg(target_os = "linux")]
#[allow(dead_code)] // XDG icon helper retained for packaging; tray embeds PNG
pub fn icon() -> Option<Icon> {
    load_icon(
        fig_util::search_xdg_data_dirs("icons/hicolor/512x512/apps/fig.png")
            .unwrap_or_else(|| "/usr/share/icons/hicolor/512x512/apps/fig.png".into()),
    )
    .or_else(load_from_memory)
}

#[cfg(all(test, not(target_os = "linux")))]
pub fn icon() -> Option<Icon> {
    load_from_memory()
}

#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn load_icon(path: impl AsRef<std::path::Path>) -> Option<Icon> {
    let image = image::open(path).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    let rgba = image.into_raw();
    Icon::from_rgba(rgba, width, height).ok()
}

#[cfg(any(target_os = "linux", test))]
#[allow(dead_code)]
fn load_from_memory() -> Option<Icon> {
    // Same bundled PNG as before; failure degrades instead of panicking.
    let image = match image::load_from_memory(include_bytes!("../icons/icon.png")) {
        Ok(image) => image.into_rgba8(),
        Err(err) => {
            warn!(?err, "failed to decode bundled icon");
            return None;
        },
    };
    let (width, height) = image.dimensions();
    match Icon::from_rgba(image.into_raw(), width, height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            warn!(?err, "failed to build bundled icon");
            None
        },
    }
}

/// A logical rect, where the origin point is the top left corner.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub position: Position,
    pub size: Size,
}

#[allow(dead_code)]
impl Rect {
    pub fn left(&self, scale_factor: f64) -> f64 {
        self.position.to_logical::<f64>(scale_factor).x
    }

    pub fn right(&self, scale_factor: f64) -> f64 {
        self.position.to_logical::<f64>(scale_factor).x + self.size.to_logical::<f64>(scale_factor).width
    }

    pub fn top(&self, scale_factor: f64) -> f64 {
        self.position.to_logical::<f64>(scale_factor).y
    }

    pub fn bottom(&self, scale_factor: f64) -> f64 {
        self.position.to_logical::<f64>(scale_factor).y + self.size.to_logical::<f64>(scale_factor).height
    }

    pub fn contains(&self, point: Position, scale_factor: f64) -> bool {
        let point = point.to_logical::<f64>(scale_factor);

        let rect_position = self.position.to_logical::<f64>(scale_factor);
        let rect_size = self.size.to_logical::<f64>(scale_factor);

        let contains_x = point.x >= rect_position.x && point.x <= rect_position.x + rect_size.width;
        let contains_y = point.y >= rect_position.y && point.y <= rect_position.y + rect_size.height;

        contains_x && contains_y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutex_lock_recovers_from_poison() {
        let mutex = Mutex::new(7u8);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("poison");
        }));
        assert_eq!(*recover_mutex(&mutex), 7);
    }

    #[test]
    fn activation_policy_and_debug_mode_recover_from_poison() {
        for (name, src) in [
            ("local_ipc/commands.rs", include_str!("local_ipc/commands.rs")),
            ("gpui_host.rs", include_str!("gpui_host.rs")),
            ("bootstrap/mod.rs", include_str!("bootstrap/mod.rs")),
            ("platform/macos.rs", include_str!("platform/macos.rs")),
        ] {
            assert!(
                !src.contains("DEBUG_MODE.lock().unwrap()"),
                "{name} still panics on DEBUG_MODE poison"
            );
            assert!(
                !src.contains("ACTIVATION_POLICY.lock().unwrap()"),
                "{name} still panics on ACTIVATION_POLICY poison"
            );
        }
        assert!(
            include_str!("local_ipc/commands.rs").contains("recover_mutex"),
            "DEBUG_MODE should recover via recover_mutex"
        );
        assert!(
            include_str!("gpui_host.rs").contains("recover_mutex")
                && include_str!("bootstrap/mod.rs").contains("recover_mutex")
                && include_str!("platform/macos.rs").contains("recover_mutex"),
            "ACTIVATION_POLICY should recover via recover_mutex"
        );
    }

    #[cfg_attr(target_os = "linux", ignore)]
    #[test]
    fn test_icon() {
        assert!(icon().is_some(), "bundled icon.png must decode");
    }

    #[test]
    fn bundled_icon_png_decodes() {
        assert!(
            load_from_memory().is_some(),
            "crates/fig_desktop/icons/icon.png must still decode on the happy path"
        );
    }
}
