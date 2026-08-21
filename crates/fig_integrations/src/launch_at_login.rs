//! Start the desktop app when the user logs in.
//!
//! macOS uses the login item. Linux writes an XDG autostart desktop file.
//! Windows writes `HKCU\...\Run`.

use std::path::{Path, PathBuf};

use fig_util::consts::APP_PROCESS_NAME;

use crate::Result;

/// Enable or disable launch at login for the current user.
pub async fn set_enabled(enabled: bool) -> Result<()> {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "macos")] {
            crate::login_item::set_enabled(enabled)
        } else if #[cfg(target_os = "linux")] {
            linux::set_enabled(enabled).await
        } else if #[cfg(windows)] {
            windows::set_enabled(enabled)
        } else {
            let _ = enabled;
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
    use fig_os_shim::{Context, FsProvider};
    use fig_util::PRODUCT_NAME;

    use super::{Result, desktop_executable, startup_command};
    use crate::desktop_entry::local_autostart_path;

    pub(super) async fn set_enabled(enabled: bool) -> Result<()> {
        let ctx = Context::new();
        let path = local_autostart_path(&ctx)?;
        let fs = ctx.fs();
        if !enabled {
            if fs.exists(&path) || fs.symlink_exists(&path).await {
                fs.remove_file(&path).await?;
            }
            return Ok(());
        }

        if let Some(parent) = path.parent() {
            if !parent.is_dir() {
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

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_VALUE: &str = "EasyComplete";

    pub(super) fn set_enabled(enabled: bool) -> Result<()> {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(RUN_KEY).map_err(winreg_err)?;
        if enabled {
            let command = startup_command(&desktop_executable());
            key.set_value(RUN_VALUE, &command).map_err(winreg_err)?;
        } else {
            let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) else {
                return Ok(());
            };
            match key.delete_value(RUN_VALUE) {
                Ok(()) => {},
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {},
                Err(err) => return Err(winreg_err(err)),
            }
        }
        Ok(())
    }

    pub(super) fn is_enabled() -> Result<bool> {
        use winreg::RegKey;
        use winreg::enums::HKEY_CURRENT_USER;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(key) = hkcu.open_subkey(RUN_KEY) else {
            return Ok(false);
        };
        let value: std::io::Result<String> = key.get_value(RUN_VALUE);
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
}
