#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Failed to open URL")]
    Failed,
}

/// `/usr/bin/open` is a process spawn so `ecterm` does not link AppKit.
/// An in-process `NSWorkspace` call pulled AppKit + Metal + IOAccelerator
/// into every tab (measured: `otool -L` listed AppKit). Compiled on every OS
/// so Linux CI pins the spawn; live `open` still needs a Mac.
#[allow(dead_code)]
const MACOS_OPEN_PROGRAM: &str = "/usr/bin/open";

/// `cmd /c start <url>`. Live spawn stays `cfg(windows)`.
#[allow(dead_code)]
const WIN32_OPEN_PROGRAM: &str = "cmd";
/// Win32 `DETACHED_PROCESS` — the console must not stay attached to `start`.
#[allow(dead_code)]
const WIN32_DETACHED_PROCESS: u32 = 0x0000_0008;
#[allow(dead_code)]
const WIN32_OPEN_ARG_C: &str = "/c";
#[allow(dead_code)]
const WIN32_OPEN_ARG_START: &str = "start";

#[allow(dead_code)]
const LINUX_OPEN_PROGRAM: &str = "xdg-open";
#[allow(dead_code)]
const WSL_OPEN_PROGRAM: &str = "wslview";

#[allow(dead_code)]
fn unix_open_program(in_wsl: bool) -> &'static str {
    if in_wsl { WSL_OPEN_PROGRAM } else { LINUX_OPEN_PROGRAM }
}

#[allow(dead_code)]
fn windows_open_args(url: &str) -> [&str; 3] {
    [WIN32_OPEN_ARG_C, WIN32_OPEN_ARG_START, url]
}

/// Build the platform opener. This crate is linked into `ecterm`, which
/// multiplies per tab, so the macOS path must stay a `/usr/bin/open` spawn.
fn open_command(url: impl AsRef<str>) -> std::process::Command {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "macos")] {
            let mut command = std::process::Command::new(MACOS_OPEN_PROGRAM);
            command.arg(url.as_ref());
            command
        } else if #[cfg(target_os = "windows")] {
            use std::os::windows::process::CommandExt;

            let mut command = std::process::Command::new(WIN32_OPEN_PROGRAM);
            command.creation_flags(WIN32_DETACHED_PROCESS);
            command.args(windows_open_args(url.as_ref()));
            command
        } else if #[cfg(any(target_os = "linux", target_os = "freebsd"))] {
            let mut command = std::process::Command::new(unix_open_program(crate::system_info::in_wsl()));
            command.arg(url.as_ref());
            command
        } else {
            compile_error!("open_url is not implemented for this target");
        }
    }
}

fn status_from_output(output: std::process::Output) -> Result<(), Error> {
    tracing::trace!(?output, "open_url output");
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Failed)
    }
}

/// Returns bool indicating whether the URL was opened successfully
pub fn open_url(url: impl AsRef<str>) -> Result<(), Error> {
    status_from_output(open_command(url).output()?)
}

/// Returns bool indicating whether the URL was opened successfully
pub async fn open_url_async(url: impl AsRef<str>) -> Result<(), Error> {
    status_from_output(tokio::process::Command::from(open_command(url)).output().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ignore]
    #[test]
    fn test_open_url() {
        open_url("https://easy-complete.emmmm.dev").unwrap();
    }

    #[test]
    fn fig_util_manifest_has_no_appkit() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.contains("objc2-app-kit")
                && !manifest.contains("macos-utils")
                && !manifest.contains("appkit-nsworkspace"),
            "AppKit on fig_util is linked into every ecterm tab"
        );
    }

    #[test]
    fn url_openers_are_process_spawns_not_appkit() {
        assert_eq!(MACOS_OPEN_PROGRAM, "/usr/bin/open");
        assert_eq!(WIN32_OPEN_PROGRAM, "cmd");
        assert_eq!(WIN32_DETACHED_PROCESS, 0x8);
        assert_eq!(
            windows_open_args("https://example.com"),
            ["/c", "start", "https://example.com"]
        );
        assert_eq!(unix_open_program(false), "xdg-open");
        assert_eq!(unix_open_program(true), "wslview");
        let production: String = include_str!("open.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//") && !line.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            production.contains("MACOS_OPEN_PROGRAM")
                && production.contains("std::process::Command::new(MACOS_OPEN_PROGRAM)"),
            "macOS must spawn /usr/bin/open, not NSWorkspace"
        );
        assert!(
            production.contains("creation_flags(WIN32_DETACHED_PROCESS)") && production.contains("windows_open_args("),
            "Windows must DETACH and use cmd /c start"
        );
        assert!(
            production.contains("unix_open_program(crate::system_info::in_wsl())"),
            "Linux/FreeBSD must pick xdg-open vs wslview from the shared helper"
        );
        assert!(
            !production.contains("NSWorkspace") && !production.contains("objc2_app_kit"),
            "do not open URLs in-process through AppKit"
        );
    }
}
