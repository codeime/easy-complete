#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Failed to open URL")]
    Failed,
}

/// Build the platform opener. This crate is linked into `ecterm`, which
/// multiplies per tab, so the macOS path must stay a `/usr/bin/open` spawn.
/// An in-process workspace call pulled AppKit + Metal into every PTY.
fn open_command(url: impl AsRef<str>) -> std::process::Command {
    cfg_if::cfg_if! {
        if #[cfg(target_os = "macos")] {
            let mut command = std::process::Command::new("/usr/bin/open");
            command.arg(url.as_ref());
            command
        } else if #[cfg(target_os = "windows")] {
            use std::os::windows::process::CommandExt;

            let detached = 0x8;
            let mut command = std::process::Command::new("cmd");
            command.creation_flags(detached);
            command.args(["/c", "start", url.as_ref()]);
            command
        } else if #[cfg(any(target_os = "linux", target_os = "freebsd"))] {
            let executable = if crate::system_info::in_wsl() {
                "wslview"
            } else {
                "xdg-open"
            };

            let mut command = std::process::Command::new(executable);
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_the_open_binary() {
        let program = format!("{:?}", open_command("https://example.com"));
        assert!(
            program.contains("/usr/bin/open"),
            "macOS opener must stay a process spawn: {program}"
        );
    }
}
