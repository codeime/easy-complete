//! Packaging-updater honesty compiled on every OS.
//!
//! Live download / `hdiutil` / AppImage swap stay behind `cfg`. Linux CI
//! pins: Windows zip/MSI is not restored, Linux tar.zst unpacking is not
//! restored, and `ec update` is macOS-only. Not live Sparkle / AppImage I/O.

#![allow(dead_code)]

pub const WINDOWS_UPDATER_UNAVAILABLE: &str = "Windows updater is not restored; zip/MSI needs a later packaging PR";

pub const LINUX_MINIMAL_UPDATER_UNAVAILABLE: &str =
    "Linux updater is not restored; tar.zst unpacking needs a later packaging PR";

pub const LINUX_FULL_UPDATER_REQUIRES_APPIMAGE: &str = "Updating is only supported from the AppImage";

pub fn cli_updates_only_on_macos(product: &str) -> String {
    format!("{product} updates are only supported on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_zip_msi_updater_is_not_restored() {
        assert!(
            WINDOWS_UPDATER_UNAVAILABLE.contains("zip/MSI") && WINDOWS_UPDATER_UNAVAILABLE.contains("not restored")
        );
        let windows = include_str!("windows.rs");
        assert!(
            windows.contains("WINDOWS_UPDATER_UNAVAILABLE") && !windows.contains("zip/MSI needs a later packaging PR"),
            "cfg(windows) updater must return the shared honesty string, not a local copy"
        );
        assert!(
            !windows.contains("sparkle") && !windows.contains("hdiutil") && !windows.contains(".msi"),
            "Windows updater must not pretend Sparkle/DMG/MSI I/O exists"
        );
    }

    #[test]
    fn linux_tar_zst_unpacking_is_not_restored() {
        assert!(
            LINUX_MINIMAL_UPDATER_UNAVAILABLE.contains("tar.zst")
                && LINUX_MINIMAL_UPDATER_UNAVAILABLE.contains("not restored")
        );
        let linux = include_str!("linux.rs");
        assert!(
            linux.contains("LINUX_MINIMAL_UPDATER_UNAVAILABLE")
                && linux.contains("LINUX_FULL_UPDATER_REQUIRES_APPIMAGE"),
            "Linux updater stubs must go through the shared honesty strings"
        );
    }

    #[test]
    fn cli_update_is_macos_only() {
        assert_eq!(
            cli_updates_only_on_macos("Easy Complete"),
            "Easy Complete updates are only supported on macOS"
        );
        let cli = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../ec_cli/src/cli/update.rs"));
        assert!(
            cli.contains("cli_updates_only_on_macos") && cli.contains("cfg!(target_os = \"macos\")"),
            "ec update must share the macOS-only message and not run a fake Linux/Windows updater"
        );
    }

    #[test]
    fn live_os_updaters_stay_cfg_gated() {
        let lib = include_str!("lib.rs");
        assert!(
            lib.contains("mod update_os_policy")
                && !lib.contains("#[cfg(windows)]\nmod update_os_policy")
                && !lib.contains("#[cfg(target_os = \"linux\")]\nmod update_os_policy"),
            "updater honesty is compiled on every OS"
        );
        assert!(
            lib.contains("#[cfg(windows)]\nmod windows")
                && lib.contains("#[cfg(target_os = \"linux\")]\nmod linux")
                && lib.contains("#[cfg(target_os = \"macos\")]\npub mod macos"),
            "live download/unpack modules stay cfg-gated"
        );
    }
}
