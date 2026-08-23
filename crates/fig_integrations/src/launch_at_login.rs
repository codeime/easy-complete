//! Start the desktop app when the user logs in.
//!
//! macOS uses the login item. Linux writes an XDG autostart desktop file.
//! Windows writes `HKCU\...\Run`.

use std::path::{Path, PathBuf};

use fig_os_shim::Context;
use fig_util::consts::APP_PROCESS_NAME;

use crate::Result;

/// Enable or disable launch at login for the current user.
pub async fn set_enabled(enabled: bool) -> Result<()> {
    set_enabled_in(&Context::new(), enabled).await
}

/// Same as [`set_enabled`], but writes through `ctx` so AppImage tests and the
/// desktop install path share one writer instead of touching the real `$HOME`.
pub async fn set_enabled_in(ctx: &Context, enabled: bool) -> Result<()> {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "macos")] {
            let _ = ctx;
            crate::login_item::set_enabled(enabled)
        } else if #[cfg(target_os = "linux")] {
            linux::set_enabled(ctx, enabled).await
        } else if #[cfg(windows)] {
            let _ = ctx;
            windows::set_enabled(enabled)
        } else {
            let _ = (ctx, enabled);
            Err(Error::Custom("launch at login is not supported on this platform".into()))
        }
    }
}

/// Whether launch at login is currently registered.
pub async fn is_enabled() -> Result<bool> {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "macos")] {
            crate::login_item::is_enabled()
        } else if #[cfg(target_os = "linux")] {
            linux::is_enabled()
        } else if #[cfg(windows)] {
            windows::is_enabled()
        } else {
            Ok(false)
        }
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn desktop_executable() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let file_name = exe.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if file_name == APP_PROCESS_NAME || file_name.starts_with("easy-complete") {
            return exe;
        }
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(APP_PROCESS_NAME);
            if candidate.exists() {
                return candidate;
            }
        }
        return exe;
    }
    PathBuf::from(APP_PROCESS_NAME)
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn startup_command(exec: &Path) -> String {
    format!("\"{}\" --is-startup", exec.display())
}

#[cfg(target_os = "linux")]
mod linux {
    use fig_os_shim::Context;
    use fig_util::PRODUCT_NAME;

    use super::{Result, desktop_executable, startup_command};
    use crate::desktop_entry::local_autostart_path;

    pub(super) async fn set_enabled(ctx: &Context, enabled: bool) -> Result<()> {
        // AppImage's FUSE mount dies with the process. Symlink to the local
        // desktop entry (Exec=$APPIMAGE) instead of writing current_exe.
        if ctx.env().in_appimage() {
            let integration = crate::desktop_entry::AutostartIntegration::new(ctx)?;
            if enabled {
                crate::Integration::install(&integration).await?;
            } else {
                crate::Integration::uninstall(&integration).await?;
            }
            return Ok(());
        }

        let path = local_autostart_path(ctx)?;
        let fs = ctx.fs();
        if !enabled {
            if fs.exists(&path) || fs.symlink_exists(&path).await {
                fs.remove_file(&path).await?;
            }
            return Ok(());
        }

        if let Some(parent) = path.parent() {
            if !fs.exists(parent) {
                fs.create_dir_all(parent).await?;
            }
        }
        let exec = desktop_executable();
        fs.write(&path, autostart_desktop_contents(&exec)).await?;
        Ok(())
    }

    pub(super) fn is_enabled() -> Result<bool> {
        let ctx = Context::new();
        Ok(ctx.fs().exists(&local_autostart_path(&ctx)?))
    }

    pub(super) fn autostart_desktop_contents(exec: &std::path::Path) -> String {
        let command = startup_command(exec);
        format!(
            "\
[Desktop Entry]
Type=Application
Name={PRODUCT_NAME}
Comment=IDE-style autocomplete for the terminal
Exec={command}
Hidden=false
X-GNOME-Autostart-enabled=true
Terminal=false
Categories=Utility;Development;
StartupWMClass=easy-complete
"
        )
    }
}

#[cfg(windows)]
mod windows {
    use super::{Result, desktop_executable, startup_command};
    use crate::Error;
    use crate::launch_at_login_policy::{WIN32_RUN_KEY, WIN32_RUN_VALUE, win32_run_delete_not_found_is_ok};

    pub(super) fn set_enabled(enabled: bool) -> Result<()> {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(WIN32_RUN_KEY).map_err(winreg_err)?;
        if enabled {
            let command = startup_command(&desktop_executable());
            key.set_value(WIN32_RUN_VALUE, &command).map_err(winreg_err)?;
        } else {
            let Ok(key) = hkcu.open_subkey_with_flags(WIN32_RUN_KEY, KEY_SET_VALUE) else {
                return Ok(());
            };
            match key.delete_value(WIN32_RUN_VALUE) {
                Ok(()) => {},
                Err(err) if win32_run_delete_not_found_is_ok(err.kind()) => {},
                Err(err) => return Err(winreg_err(err)),
            }
        }
        Ok(())
    }

    pub(super) fn is_enabled() -> Result<bool> {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(key) = hkcu.open_subkey(WIN32_RUN_KEY) else {
            return Ok(false);
        };
        let value: std::io::Result<String> = key.get_value(WIN32_RUN_VALUE);
        Ok(value.is_ok())
    }

    fn winreg_err(err: impl std::fmt::Display) -> Error {
        Error::Custom(err.to_string().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_command_quotes_the_binary_and_marks_login() {
        let command = startup_command(Path::new(r"C:\Program Files\easy-complete.exe"));
        assert!(command.starts_with('"'), "{command}");
        assert!(command.contains("--is-startup"), "{command}");
        assert!(command.contains("easy-complete.exe"), "{command}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_autostart_file_launches_with_is_startup() {
        let contents = linux::autostart_desktop_contents(Path::new("/usr/local/bin/easy-complete"));
        assert!(contents.contains("Exec=\"/usr/local/bin/easy-complete\" --is-startup"));
        assert!(contents.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn appimage_autostart_symlinks_the_local_desktop_entry() {
        use crate::Integration;
        use crate::desktop_entry::{AutostartIntegration, local_entry_path};

        let ctx = Context::builder()
            .with_test_home()
            .await
            .unwrap()
            .with_env_var("APPIMAGE", "/test.appimage")
            .build_fake();
        let local = local_entry_path(&ctx).unwrap();
        ctx.fs().create_dir_all(local.parent().unwrap()).await.unwrap();
        ctx.fs().write(&local, "[Desktop Entry]").await.unwrap();

        set_enabled_in(&ctx, true).await.unwrap();
        AutostartIntegration::to_local(&ctx)
            .unwrap()
            .is_installed()
            .await
            .unwrap();

        set_enabled_in(&ctx, false).await.unwrap();
        assert!(
            AutostartIntegration::to_local(&ctx)
                .unwrap()
                .is_installed()
                .await
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prefix_autostart_writes_is_startup_into_the_fake_home() {
        use crate::desktop_entry::local_autostart_path;

        let ctx = Context::builder().with_test_home().await.unwrap().build_fake();
        set_enabled_in(&ctx, true).await.unwrap();
        let path = local_autostart_path(&ctx).unwrap();
        let contents = ctx.fs().read_to_string(&path).await.unwrap();
        assert!(contents.contains("--is-startup"), "{contents}");
        set_enabled_in(&ctx, false).await.unwrap();
        assert!(!ctx.fs().exists(&path));
    }
}
