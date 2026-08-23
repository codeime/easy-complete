use std::convert::TryInto;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use camino::Utf8PathBuf;
use fig_os_shim::{Context, EnvProvider, FsProvider, Os, PlatformProvider, Shim};
use thiserror::Error;
use time::OffsetDateTime;

#[cfg(unix)]
use crate::RUNTIME_DIR_NAME;
use crate::consts::linux::DESKTOP_ENTRY_NAME;
use crate::env_var::{Q_BUNDLE_METADATA_PATH, Q_PARENT};
use crate::system_info::{in_cloudshell, is_remote};
use crate::{APP_PROCESS_NAME, BACKUP_DIR_NAME, DATA_DIR_NAME, TAURI_PRODUCT_NAME};

macro_rules! utf8_dir {
    ($name:ident, $($arg:ident: $type:ty),*) => {
        paste::paste! {
            pub fn [<$name _utf8>]($($arg: $type),*) -> Result<Utf8PathBuf> {
                Ok($name($($arg),*)?.try_into()?)
            }
        }
    };
    ($name:ident) => {
        utf8_dir!($name,);
    };
}

#[derive(Debug, Error)]
pub enum DirectoryError {
    #[error("home directory not found")]
    NoHomeDirectory,
    #[error("runtime directory not found: neither XDG_RUNTIME_DIR nor TMPDIR were found")]
    NoRuntimeDirectory,
    #[error("non absolute path: {0:?}")]
    NonAbsolutePath(PathBuf),
    #[error("unsupported platform: {0:?}")]
    UnsupportedOs(Os),
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    TimeFormat(#[from] time::error::Format),
    #[error(transparent)]
    Utf8FromPath(#[from] camino::FromPathError),
    #[error(transparent)]
    Utf8FromPathBuf(#[from] camino::FromPathBufError),
    #[error(transparent)]
    FromVecWithNul(#[from] std::ffi::FromVecWithNulError),
    #[error(transparent)]
    IntoString(#[from] std::ffi::IntoStringError),
    #[error("{Q_PARENT} env variable not set")]
    QParentNotSet,
    #[error("must be ran from an appimage executable")]
    NotAppImage,
}

type Result<T, E = DirectoryError> = std::result::Result<T, E>;

/// The directory of the users home
///
/// - Linux: /home/Alice
/// - MacOS: /Users/Alice
/// - Windows: C:\Users\Alice
pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or(DirectoryError::NoHomeDirectory)
}

pub fn home_dir_ctx<Ctx: FsProvider + EnvProvider>(ctx: &Ctx) -> Result<PathBuf> {
    if ctx.env().is_real() {
        home_dir()
    } else {
        ctx.env()
            .get("HOME")
            .map_err(|_err| DirectoryError::NoHomeDirectory)
            .and_then(|h| {
                if h.is_empty() {
                    Err(DirectoryError::NoHomeDirectory)
                } else {
                    Ok(h)
                }
            })
            .map(PathBuf::from)
            .map(|p| ctx.fs().chroot_path(p))
    }
}

/// The directory of the users `$HOME/.local/bin` directory
///
/// MacOS and Linux path: `$HOME/.local/bin``
#[cfg(unix)]
pub fn home_local_bin() -> Result<PathBuf> {
    let mut path = home_dir()?;
    path.push(".local/bin");
    Ok(path)
}

#[cfg(unix)]
pub fn home_local_bin_ctx(ctx: &Context) -> Result<PathBuf> {
    let mut path = home_dir_ctx(ctx)?;
    path.push(".local/bin");
    Ok(path)
}

/// The config directory
///
/// - Linux: `$XDG_CONFIG_HOME` or `$HOME/.config`
/// - MacOS: `$HOME/Library/Application Support`
/// - Windows: `{FOLDERID_RoamingAppData}`
pub fn config_dir() -> Result<PathBuf> {
    dirs::config_dir().ok_or(DirectoryError::NoHomeDirectory)
}

/// The old codewhisperer data directory
///
/// This should be removed at some point in the future, once all our users have migrated
/// - MacOS: `$HOME/Library/Application Support/codewhisperer`
pub fn old_fig_data_dir() -> Result<PathBuf> {
    Ok(dirs::data_local_dir()
        .ok_or(DirectoryError::NoHomeDirectory)?
        .join("codewhisperer"))
}

/// The q data directory
///
/// - Linux: `$XDG_DATA_HOME/{data_dir}` or `$HOME/.local/share/{data_dir}`
/// - MacOS: `$HOME/Library/Application Support/{data_dir}`
/// - Windows: `%LOCALAPPDATA%\{data_dir}`
pub fn fig_data_dir() -> Result<PathBuf> {
    Ok(dirs::data_local_dir()
        .ok_or(DirectoryError::NoHomeDirectory)?
        .join(DATA_DIR_NAME))
}

pub fn fig_data_dir_ctx(fs: &impl FsProvider) -> Result<PathBuf> {
    Ok(fs.fs().chroot_path(fig_data_dir()?))
}

/// The user's local data directory.
///
/// - Linux: `$XDG_DATA_HOME` or `$HOME/.local/share`
/// - MacOS: `$HOME/Library/Application Support`
/// - Windows: `%LOCALAPPDATA%`
pub fn local_data_dir<Ctx: FsProvider + EnvProvider + PlatformProvider>(ctx: &Ctx) -> Result<PathBuf> {
    let env = ctx.env();
    match ctx.platform().os() {
        Os::Linux => {
            if let Some(path) = env.get_os("XDG_DATA_HOME") {
                return Ok(path.into());
            }
            Ok(home_dir_ctx(ctx)?.join(".local/share"))
        },
        Os::Mac => Ok(home_dir_ctx(ctx)?.join("Library/Application Support")),
        Os::Windows => {
            if let Some(path) = env.get_os("LOCALAPPDATA") {
                return Ok(path.into());
            }
            Ok(home_dir_ctx(ctx)?.join("AppData").join("Local"))
        },
        os => Err(DirectoryError::UnsupportedOs(os)),
    }
}

/// The q cache directory
///
/// - Linux: `$XDG_CACHE_HOME/{data_dir}` or `$HOME/.cache/{data_dir}`
/// - MacOS: `$HOME/Library/Caches/{data_dir}`
/// - Windows: `%LOCALAPPDATA%\{data_dir}\cache`
pub fn cache_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .ok_or(DirectoryError::NoHomeDirectory)?
        .join(DATA_DIR_NAME))
}

/// Get the macos tempdir from the `confstr` function
///
/// See: <https://man7.org/linux/man-pages/man3/confstr.3.html>
#[cfg(target_os = "macos")]
fn macos_tempdir() -> Result<PathBuf> {
    let len = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null::<i8>().cast_mut(), 0) };
    let mut buf: Vec<u8> = vec![0; len];
    unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    let c_string = std::ffi::CString::from_vec_with_nul(buf)?;
    let str = c_string.into_string()?;
    Ok(PathBuf::from(str))
}

/// Runtime dir is used for runtime data that should not be persisted for a long time, e.g. socket
/// files and logs
///
/// The XDG_RUNTIME_DIR is set by systemd <https://www.freedesktop.org/software/systemd/man/latest/file-hierarchy.html#/run/user/>,
/// if this is not set such as on macOS it will fallback to TMPDIR which is secure on macOS
/// On Windows, it uses the TEMP directory
pub fn runtime_dir() -> Result<PathBuf> {
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            let mut dir = dirs::runtime_dir();
            dir = dir.or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from));

            cfg_if::cfg_if! {
                if #[cfg(target_os = "macos")] {
                    let macos_tempdir = macos_tempdir()?;
                    dir = dir.or(Some(macos_tempdir));
                } else {
                    dir = dir.or_else(|| Some(std::env::temp_dir()));
                }
            }

            dir.ok_or(DirectoryError::NoRuntimeDirectory)
        } else if #[cfg(windows)] {
            Ok(std::env::temp_dir())
        }
    }
}

/// Windows nested layout under `%TEMP%`. Compiled on every OS so Linux CI can
/// pin `%TEMP%\easy-complete\{sockets,logs}` without a Windows host.
#[cfg(any(test, windows))]
fn windows_temp_child(temp: &Path, leaf: &str) -> PathBuf {
    temp.join(DATA_DIR_NAME).join(leaf)
}

/// The q sockets directory of the local q installation
///
/// - Linux: $XDG_RUNTIME_DIR/ecrun
/// - MacOS: $TMPDIR/ecrun
/// - Windows: %TEMP%\{data_dir}\sockets
pub fn sockets_dir() -> Result<PathBuf> {
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            Ok(runtime_dir()?.join(RUNTIME_DIR_NAME))
        } else if #[cfg(windows)] {
            Ok(windows_temp_child(&runtime_dir()?, "sockets"))
        }
    }
}

/// The directory on the host machine where socket files are stored
///
/// In WSL, this will correctly return the host machine socket path.
/// In other remote environments, it returns the same as `sockets_dir`
///
/// - Linux: $XDG_RUNTIME_DIR/ecrun
/// - MacOS: $TMPDIR/ecrun
/// - Windows: %TEMP%\{data_dir}\sockets (same as [`sockets_dir`])
pub fn host_sockets_dir() -> Result<PathBuf> {
    // TODO: make this work again
    // #[cfg(target_os = "linux")]
    // if crate::system_info::in_wsl() {
    //     use std::ffi::OsStr;
    //     use std::os::unix::prelude::OsStrExt;
    //     use std::process::Command;

    //     use bstr::ByteSlice;

    //     let socket_dir = Command::new("fig.exe").args(["_", "sockets-dir"]).output()?;
    //     let wsl_socket = Command::new("wslpath")
    //         .arg(OsStr::from_bytes(socket_dir.stdout.trim()))
    //         .output()?;
    //     return Ok(PathBuf::from(OsStr::from_bytes(wsl_socket.stdout.trim())));
    // }

    sockets_dir()
}

/// The path to all of the themes
pub fn themes_dir(ctx: &Context) -> Result<PathBuf> {
    Ok(resources_path_ctx(ctx)?.join("themes"))
}

/// The directory to all the fig logs
/// - Linux: `$XDG_RUNTIME_DIR/eclog` (then `$TMPDIR` / `/tmp`)
/// - MacOS: `$TMPDIR/eclog`
/// - Windows: `%TEMP%\{data_dir}\logs`
pub fn logs_dir() -> Result<PathBuf> {
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            use crate::CLI_BINARY_NAME;
            Ok(runtime_dir()?.join(format!("{CLI_BINARY_NAME}log")))
        } else if #[cfg(windows)] {
            Ok(windows_temp_child(&std::env::temp_dir(), "logs"))
        }
    }
}

/// The directory where fig places all data-sensitive backups
///
/// - Linux/MacOS: `$HOME/{backup_dir}`
/// - Windows: `%USERPROFILE%\{backup_dir}`
pub fn backups_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(BACKUP_DIR_NAME))
}

/// The directory for time based data-sensitive backups
///
/// NOTE: This changes every second and cannot be cached
pub fn utc_backup_dir() -> Result<PathBuf> {
    let now = OffsetDateTime::now_utc().format(time::macros::format_description!(
        "[year]-[month]-[day]_[hour]-[minute]-[second]"
    ))?;

    Ok(backups_dir()?.join(now))
}

/// The directory to the directory containing config for the `/context` feature in `q chat`.
pub fn chat_global_context_path<Ctx: FsProvider + EnvProvider>(ctx: &Ctx) -> Result<PathBuf> {
    Ok(home_dir_ctx(ctx)?
        .join(".aws")
        .join("amazonq")
        .join("global_context.json"))
}

/// The directory to the directory containing config for the `/context` feature in `q chat`.
pub fn chat_profiles_dir<Ctx: FsProvider + EnvProvider>(ctx: &Ctx) -> Result<PathBuf> {
    Ok(home_dir_ctx(ctx)?.join(".aws").join("amazonq").join("profiles"))
}

/// The desktop app socket path
///
/// - MacOS: `$TMPDIR/ecrun/desktop.sock`
/// - Linux: `$XDG_RUNTIME_DIR/ecrun/desktop.sock`
/// - Windows: `%TEMP%\{data_dir}\sockets\desktop.sock`
pub fn desktop_socket_path() -> Result<PathBuf> {
    Ok(host_sockets_dir()?.join("desktop.sock"))
}

/// The path to remote socket
// - Linux/MacOS on ssh: At the value of `Q_PARENT`
// - Linux/MacOS not on ssh:
/// - MacOS: `$TMPDIR/ecrun/remote.sock`
/// - Linux: `$XDG_RUNTIME_DIR/ecrun/remote.sock`
/// - Windows: `%TEMP%\{data_dir}\sockets\remote.sock`
pub fn remote_socket_path() -> Result<PathBuf> {
    // Normal implementation for non-test code
    // TODO(grant): This is only enabled on Linux for now to prevent public dist
    if is_remote() && !in_cloudshell() && cfg!(target_os = "linux") {
        if let Some(parent_socket) = fig_os_shim::Env::new().get_os(Q_PARENT) {
            Ok(PathBuf::from(parent_socket))
        } else {
            Err(DirectoryError::QParentNotSet)
        }
    } else {
        local_remote_socket_path()
    }
}

/// The path to local remote socket
///
/// - MacOS: `$TMPDIR/ecrun/remote.sock`
/// - Linux: `$XDG_RUNTIME_DIR/ecrun/remote.sock`
/// - Windows: `%TEMP%\{data_dir}\sockets\remote.sock`
pub fn local_remote_socket_path() -> Result<PathBuf> {
    Ok(host_sockets_dir()?.join("remote.sock"))
}

/// Get path to a figterm socket
///
/// - MacOS: `$TMPDIR/ecrun/t/$SESSION_ID.sock`
/// - Linux: `$XDG_RUNTIME_DIR/ecrun/t/$SESSION_ID.sock`
/// - Windows: `%TEMP%\{data_dir}\sockets\t\$SESSION_ID.sock`
pub fn figterm_socket_path(session_id: impl Display) -> Result<PathBuf> {
    Ok(sockets_dir()?.join("t").join(format!("{session_id}.sock")))
}

/// The path to the resources directory
///
/// - MacOS: "/Applications/{app_name}.app/Contents/Resources"
/// - Linux: `$PREFIX/share/easy-complete` (exe-relative, then `XDG_DATA_DIRS`, then `/usr/share`)
/// - Windows: "%LOCALAPPDATA%\{data_dir}\resources"
pub fn resources_path() -> Result<PathBuf> {
    cfg_if::cfg_if! {
        if #[cfg(all(unix, not(target_os = "macos")))] {
            Ok(linux_share_dir_from_std_env())
        } else if #[cfg(target_os = "macos")] {
            Ok(crate::app_bundle_path().join(crate::macos::BUNDLE_CONTENTS_RESOURCE_PATH))
        } else if #[cfg(windows)] {
            Ok(fig_data_dir()?.join("resources"))
        }
    }
}

pub fn resources_path_ctx<Ctx: EnvProvider + PlatformProvider>(ctx: &Ctx) -> Result<PathBuf> {
    let os = ctx.platform().os();
    match os {
        fig_os_shim::Os::Mac => Ok(crate::app_bundle_path().join(crate::macos::BUNDLE_CONTENTS_RESOURCE_PATH)),
        fig_os_shim::Os::Linux => {
            if ctx.env().in_appimage() {
                Ok(ctx
                    .env()
                    .current_dir()?
                    .join(format!("lib/{}", TAURI_PRODUCT_NAME.replace("_", "-"))))
            } else {
                Ok(linux_share_dir_from_env(
                    |key| ctx.env().get(key),
                    ctx.env().current_exe().ok(),
                ))
            }
        },
        fig_os_shim::Os::Windows => Ok(fig_data_dir()?.join("resources")),
        _ => Err(DirectoryError::UnsupportedOs(os)),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn linux_share_dir_from_std_env() -> PathBuf {
    // Wrap `std::env::var` in a closure so the `Fn(&str)` bound is HRTB-general
    // (`std::env::var` alone is a polymorphic fn item and fails on Linux rustc).
    linux_share_dir_from_env(|key| std::env::var(key), std::env::current_exe().ok())
}

fn linux_share_dir_from_env(
    env_var: impl Fn(&str) -> Result<String, std::env::VarError>,
    current_exe: Option<PathBuf>,
) -> PathBuf {
    for dir in linux_share_dir_candidates(env_var, current_exe.as_deref()) {
        if dir.is_dir() {
            return dir;
        }
    }
    PathBuf::from("/usr/share").join(DATA_DIR_NAME)
}

fn linux_share_dir_candidates(
    env_var: impl Fn(&str) -> Result<String, std::env::VarError>,
    current_exe: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(bin) = current_exe.and_then(Path::parent) {
        dirs.push(bin.join(format!("../share/{DATA_DIR_NAME}")));
    }
    if let Ok(xdg_home) = env_var("XDG_DATA_HOME") {
        if !xdg_home.is_empty() {
            dirs.push(PathBuf::from(xdg_home).join(DATA_DIR_NAME));
        }
    }
    let xdg_dirs = env_var("XDG_DATA_DIRS")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    for dir in xdg_dirs.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir).join(DATA_DIR_NAME));
    }
    dirs.push(PathBuf::from("/usr/share").join(DATA_DIR_NAME));
    dirs
}

/// The path to the fig install manifest
///
/// - MacOS: "/Applications/{app_name}.app/Contents/Resources/manifest.json"
/// - Linux: "/usr/share/easy-complete/manifest.json"
/// - Windows: "%LOCALAPPDATA%\{data_dir}\resources\bin\manifest.json"
pub fn manifest_path() -> Result<PathBuf> {
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            Ok(resources_path()?.join("manifest.json"))
        } else if #[cfg(target_os = "windows")] {
            Ok(resources_path()?.join("bin").join("manifest.json"))
        }
    }
}

/// The path to the metadata.json file included with a Linux desktop bundle.
///
/// This should only be called from the desktop binary since AppImage bundles can only access the
/// resources directory from the AppImage mount, known only by the AppImage itself (ie, the desktop
/// binary).
pub fn bundle_metadata_path<Ctx: EnvProvider + PlatformProvider>(ctx: &Ctx) -> Result<PathBuf> {
    if let Some(path) = ctx.env().get_os(Q_BUNDLE_METADATA_PATH) {
        return Ok(path.into());
    }
    Ok(resources_path_ctx(ctx)?.join("bundle-metadata").join("metadata.json"))
}

/// The path to the fig settings file
///
/// - Linux: `$HOME/.local/share/{data_dir}/settings.json`
/// - MacOS: `$HOME/Library/Application Support/{data_dir}/settings.json`
/// - Windows: `%LOCALAPPDATA%\{data_dir}\settings.json`
pub fn settings_path() -> Result<PathBuf> {
    Ok(fig_data_dir()?.join("settings.json"))
}

/// The path to the lock file used to indicate that the app is updating
///
/// - Linux: `$HOME/.local/share/{data_dir}/update.lock`
/// - MacOS: `$HOME/Library/Application Support/{data_dir}/update.lock`
/// - Windows: `%LOCALAPPDATA%\{data_dir}\update.lock`
pub fn update_lock_path(ctx: &impl FsProvider) -> Result<PathBuf> {
    Ok(fig_data_dir_ctx(ctx)?.join("update.lock"))
}

/// The path to the desktop entry bundled with the AppImage.
///
/// Only applicable to the desktop app binary when ran as an AppImage.
pub fn appimage_desktop_entry_path<Ctx: EnvProvider>(ctx: &Ctx) -> Result<PathBuf> {
    if !ctx.env().in_appimage() {
        return Err(DirectoryError::NotAppImage);
    }
    Ok(ctx
        .env()
        .current_dir()?
        .join("share/applications")
        .join(DESKTOP_ENTRY_NAME))
}

/// The path to the icon bundled with the AppImage to be used for the desktop entry file.
///
/// Only applicable to the desktop app binary when ran as an AppImage.
pub fn appimage_desktop_entry_icon_path<Ctx: EnvProvider>(ctx: &Ctx) -> Result<PathBuf> {
    if !ctx.env().in_appimage() {
        return Err(DirectoryError::NotAppImage);
    }
    Ok(ctx
        .env()
        .current_dir()?
        .join(format!("share/icons/hicolor/128x128/apps/{APP_PROCESS_NAME}.png")))
}

utf8_dir!(home_dir);
#[cfg(unix)]
utf8_dir!(home_local_bin);
utf8_dir!(fig_data_dir);
utf8_dir!(sockets_dir);
utf8_dir!(remote_socket_path);
utf8_dir!(figterm_socket_path, session_id: impl Display);
utf8_dir!(manifest_path);
utf8_dir!(backups_dir);
utf8_dir!(logs_dir);
utf8_dir!(settings_path);

#[cfg(test)]
mod linux_tests {
    use std::path::PathBuf;

    use super::*;
    use crate::DATA_DIR_NAME;

    #[test]
    fn all_paths() {
        let ctx = Context::new();
        assert!(home_dir().is_ok());
        #[cfg(unix)]
        assert!(home_local_bin().is_ok());
        assert!(fig_data_dir().is_ok());
        assert!(sockets_dir().is_ok());
        assert!(remote_socket_path().is_ok());
        assert!(local_remote_socket_path().is_ok());
        assert!(figterm_socket_path("test").is_ok());
        assert!(resources_path().is_ok());
        assert!(manifest_path().is_ok());
        assert!(backups_dir().is_ok());
        assert!(logs_dir().is_ok());
        assert!(settings_path().is_ok());
        assert!(update_lock_path(&ctx).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn sockets_live_under_runtime_dir_named_ecrun() {
        let sockets = sockets_dir().unwrap();
        assert_eq!(sockets.file_name().and_then(|n| n.to_str()), Some(RUNTIME_DIR_NAME));
        assert_eq!(sockets.parent(), Some(runtime_dir().unwrap().as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn logs_live_under_runtime_dir_named_eclog() {
        let logs = logs_dir().unwrap();
        assert_eq!(
            logs.file_name().and_then(|n| n.to_str()),
            Some("eclog"),
            "CLI_BINARY_NAME + \"log\""
        );
        assert_eq!(logs.parent(), Some(runtime_dir().unwrap().as_path()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sockets_follow_xdg_when_set() {
        let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") else {
            return;
        };
        if xdg.is_empty() {
            return;
        }
        let sockets = sockets_dir().unwrap();
        assert!(
            sockets.starts_with(&xdg),
            "sockets_dir {sockets:?} should be under XDG_RUNTIME_DIR {xdg}"
        );
        assert_eq!(sockets.file_name().and_then(|n| n.to_str()), Some("ecrun"));
    }

    #[test]
    fn windows_temp_layout_nests_under_data_dir() {
        let temp = PathBuf::from(r"C:\Users\Ada\AppData\Local\Temp");
        assert_eq!(
            windows_temp_child(&temp, "sockets"),
            temp.join(DATA_DIR_NAME).join("sockets")
        );
        assert_eq!(windows_temp_child(&temp, "logs"), temp.join(DATA_DIR_NAME).join("logs"));
    }

    #[test]
    fn linux_data_prefix_is_easy_complete_not_a_webview_path() {
        assert_eq!(DATA_DIR_NAME, "easy-complete");
        assert_eq!(crate::consts::linux::PACKAGE_NAME, DATA_DIR_NAME);
    }

    #[test]
    fn appimage_bundle_paths_use_product_desktop_entry_names() {
        assert_eq!(DESKTOP_ENTRY_NAME, "easy-complete.desktop");
        assert!(
            !DESKTOP_ENTRY_NAME.contains("q-desktop"),
            "AppImage desktop entry must not keep the q-desktop filename"
        );
        let src = include_str!("directories.rs");
        let old_desktop = ["q-desktop", ".desktop"].concat();
        let old_icon = ["q-desktop", ".png"].concat();
        assert!(
            !src.contains(&old_desktop) && !src.contains(&old_icon),
            "AppImage path helpers must not hardcode q-desktop filenames"
        );
        assert!(src.contains("DESKTOP_ENTRY_NAME") && src.contains("APP_PROCESS_NAME"));
    }
}

// TODO(grant): Add back path tests on linux
#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use insta;

    use super::*;

    /// If this test fails then either of these paths were changed.
    ///
    /// Since we set the permissions of the parent of these paths, make sure they're in folders we
    /// own otherwise we will set permissions of directories we shouldn't
    #[test]
    fn test_socket_paths() {
        #[cfg(unix)]
        assert_eq!(
            host_sockets_dir().unwrap().file_name().unwrap().to_str().unwrap(),
            format!("ecrun")
        );

        #[cfg(windows)]
        assert_eq!(
            host_sockets_dir().unwrap().file_name().unwrap().to_str().unwrap(),
            format!("sockets")
        );

        #[cfg(unix)]
        assert_eq!(
            figterm_socket_path("").unwrap().parent().unwrap().file_name().unwrap(),
            "t"
        );

        #[cfg(windows)]
        assert_eq!(
            figterm_socket_path("").unwrap().parent().unwrap().file_name().unwrap(),
            "t"
        );
    }

    macro_rules! assert_directory {
        ($value:expr, @$snapshot:literal) => {
            insta::assert_snapshot!(
                sanitized_directory_path($value),
                @$snapshot,
            )
        };
    }

    macro_rules! macos {
        ($value:expr, @$snapshot:literal) => {
            #[cfg(target_os = "macos")]
            assert_directory!($value, @$snapshot)
        };
    }

    macro_rules! linux {
        ($value:expr, @$snapshot:literal) => {
            #[cfg(target_os = "linux")]
            assert_directory!($value, @$snapshot)
        };
    }

    macro_rules! windows {
        ($value:expr, @$snapshot:literal) => {
            #[cfg(target_os = "windows")]
            assert_directory!($value, @$snapshot)
        };
    }

    fn sanitized_directory_path(path: Result<PathBuf>) -> String {
        let mut path = path.unwrap().into_os_string().into_string().unwrap();

        if let Ok(home) = std::env::var("HOME") {
            let home = home.strip_suffix('/').unwrap_or(&home);
            path = path.replace(home, "$HOME");
        }

        if let Ok(user) = whoami::username() {
            path = path.replace(&user, "$USER");
        }

        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            let tmpdir = tmpdir.strip_suffix('/').unwrap_or(&tmpdir);
            path = path.replace(tmpdir, "$TMPDIR");
        }

        #[cfg(target_os = "macos")]
        {
            if let Ok(tmpdir) = macos_tempdir() {
                let tmpdir = tmpdir.to_str().unwrap();
                let tmpdir = tmpdir.strip_suffix('/').unwrap_or(tmpdir);
                path = path.replace(tmpdir, "$TMPDIR");
            };
        }

        if let Ok(xdg_runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let xdg_runtime_dir = xdg_runtime_dir.strip_suffix('/').unwrap_or(&xdg_runtime_dir);
            path = path.replace(xdg_runtime_dir, "$XDG_RUNTIME_DIR");
        }

        #[cfg(target_os = "linux")]
        {
            path = path.replace("/tmp", "$TMPDIR");
        }

        path
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_home_local_bin() {
        #[cfg(target_os = "linux")]
        linux!(home_local_bin(), @"$HOME/.local/bin");
        #[cfg(target_os = "macos")]
        macos!(home_local_bin(), @"$HOME/.local/bin");
    }

    #[test]
    fn snapshot_fig_data_dir() {
        linux!(fig_data_dir(), @"$HOME/.local/share/easy-complete");
        macos!(fig_data_dir(), @"$HOME/Library/Application Support/easy-complete");
        windows!(fig_data_dir(), @r"C:\Users\$USER\AppData\Local\easy-complete");
    }

    #[test]
    fn snapshot_sockets_dir() {
        linux!(sockets_dir(), @"$XDG_RUNTIME_DIR/ecrun");
        macos!(sockets_dir(), @"$TMPDIR/ecrun");
        windows!(sockets_dir(), @r"C:\Users\$USER\AppData\Local\Temp\easy-complete\sockets");
    }

    #[test]
    fn snapshot_themes_dir() {
        linux!(themes_dir(&Context::new()), @"/usr/share/easy-complete/themes");
        macos!(themes_dir(&Context::new()), @"/Applications/Easy Complete.app/Contents/Resources/themes");
        windows!(themes_dir(&Context::new()), @r"C:\Users\$USER\AppData\Local\easy-complete\resources\themes");
    }

    #[test]
    fn linux_share_looks_beside_prefix_bin() {
        let dirs = linux_share_dir_candidates(
            |_| Err(std::env::VarError::NotPresent),
            Some(Path::new("/usr/local/bin/easy-complete")),
        );
        assert_eq!(
            dirs.first().map(PathBuf::as_path),
            Some(Path::new("/usr/local/bin/../share/easy-complete"))
        );
        assert_eq!(
            dirs.last().map(PathBuf::as_path),
            Some(Path::new("/usr/share/easy-complete"))
        );
    }

    #[test]
    fn snapshot_backups_dir() {
        linux!(backups_dir(), @"$HOME/.easy-complete.dotfiles.bak");
        macos!(backups_dir(), @"$HOME/.easy-complete.dotfiles.bak");
        windows!(backups_dir(), @r"C:\Users\$USER\.easy-complete.dotfiles.bak");
    }

    #[test]
    fn snapshot_fig_socket_path() {
        linux!(desktop_socket_path(), @"$XDG_RUNTIME_DIR/ecrun/desktop.sock");
        macos!(desktop_socket_path(), @"$TMPDIR/ecrun/desktop.sock");
        windows!(desktop_socket_path(), @r"C:\Users\$USER\AppData\Local\Temp\easy-complete\sockets\desktop.sock");
    }

    #[test]
    fn snapshot_remote_socket_path() {
        linux!(remote_socket_path(), @"$XDG_RUNTIME_DIR/ecrun/remote.sock");
        macos!(remote_socket_path(), @"$TMPDIR/ecrun/remote.sock");
        windows!(remote_socket_path(), @r"C:\Users\$USER\AppData\Local\Temp\easy-complete\sockets\remote.sock");
    }

    #[test]
    fn snapshot_local_remote_socket_path() {
        linux!(local_remote_socket_path(), @"$XDG_RUNTIME_DIR/ecrun/remote.sock");
        macos!(local_remote_socket_path(), @"$TMPDIR/ecrun/remote.sock");
        windows!(local_remote_socket_path(), @r"C:\Users\$USER\AppData\Local\Temp\easy-complete\sockets\remote.sock");
    }

    #[test]
    fn snapshot_figterm_socket_path() {
        linux!(figterm_socket_path("$SESSION_ID"), @"$XDG_RUNTIME_DIR/ecrun/t/$SESSION_ID.sock");
        macos!(figterm_socket_path("$SESSION_ID"), @"$TMPDIR/ecrun/t/$SESSION_ID.sock");
        windows!(figterm_socket_path("$SESSION_ID"), @r"C:\Users\$USER\AppData\Local\Temp\easy-complete\sockets\t\$SESSION_ID.sock");
    }

    #[test]
    fn snapshot_settings_path() {
        linux!(settings_path(), @"$HOME/.local/share/easy-complete/settings.json");
        macos!(settings_path(), @"$HOME/Library/Application Support/easy-complete/settings.json");
        windows!(settings_path(), @r"C:\Users\$USER\AppData\Local\easy-complete\settings.json");
    }

    #[test]
    fn snapshot_update_lock_path() {
        let ctx = Context::new();
        linux!(update_lock_path(&ctx), @"$HOME/.local/share/easy-complete/update.lock");
        macos!(update_lock_path(&ctx), @"$HOME/Library/Application Support/easy-complete/update.lock");
        windows!(update_lock_path(&ctx), @r"C:\Users\$USER\AppData\Local\easy-complete\update.lock");
    }

    #[test]
    #[cfg(unix)]
    fn socket_path_length() {
        use std::os::unix::ffi::OsStrExt;
        /// Sockets are bounded at 100 bytes, why, because legacy compat
        const MAX_SOCKET_LEN: usize = 100;

        let uuid = uuid::Uuid::new_v4().simple().to_string();
        let qterm_socket = figterm_socket_path(uuid.clone()).unwrap();
        let qterm_socket_bytes = qterm_socket.as_os_str().as_bytes().len();
        assert!(qterm_socket_bytes <= MAX_SOCKET_LEN);

        let fig_socket = desktop_socket_path().unwrap();
        let fig_socket_bytes = fig_socket.as_os_str().as_bytes().len();
        assert!(fig_socket_bytes <= MAX_SOCKET_LEN);

        let secure_socket = remote_socket_path().unwrap();
        let secure_socket_bytes = secure_socket.as_os_str().as_bytes().len();
        assert!(secure_socket_bytes <= MAX_SOCKET_LEN);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_tempdir_test() {
        let tmpdir = macos_tempdir().unwrap();
        println!("{:?}", tmpdir);
    }
}
