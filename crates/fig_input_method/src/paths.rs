//! The two paths this process needs, derived the same way `fig_util::directories`
//! derives them.
//!
//! Reimplemented rather than imported: `fig_util` pulls tokio, regex, sysinfo and
//! `macos-utils`. [`tests::paths_match_fig_util`] compares both against the
//! originals, so a change over there fails here instead of silently sending the
//! caret to a path nobody is listening on.

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::OnceLock;

const RUNTIME_DIR_NAME: &str = "ecrun";
const LOG_DIR_NAME: &str = "eclog";
const DESKTOP_SOCKET_NAME: &str = "desktop.sock";
const LOG_FILE_NAME: &str = "imk.log";

fn darwin_user_temp_dir() -> Option<PathBuf> {
    let len = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null::<i8>().cast_mut(), 0) };
    if len == 0 {
        return None;
    }
    let mut buf: Vec<u8> = vec![0; len];
    unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    let path = CString::from_vec_with_nul(buf).ok()?.into_string().ok()?;
    Some(PathBuf::from(path))
}

/// `dirs::runtime_dir()` is `XDG_RUNTIME_DIR`, which macOS never sets, so
/// `fig_util` falls through to `TMPDIR` and then to the per-user Darwin temp dir.
fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .or_else(darwin_user_temp_dir)
}

/// Cached: the caret path is walked once per keystroke and `TMPDIR` cannot change
/// under a running process.
pub fn desktop_socket_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| Some(runtime_dir()?.join(RUNTIME_DIR_NAME).join(DESKTOP_SOCKET_NAME)))
        .as_ref()
}

pub fn log_file_path() -> Option<PathBuf> {
    Some(runtime_dir()?.join(LOG_DIR_NAME).join(LOG_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The TMPDIR branch is what `paths_match_fig_util` exercises (macOS always
    /// sets it); this covers the launchd-without-TMPDIR fallback.
    #[test]
    fn darwin_user_temp_dir_resolves() {
        let dir = darwin_user_temp_dir().unwrap();
        assert!(dir.is_absolute());
        assert!(dir.is_dir());
    }

    #[test]
    fn paths_match_fig_util() {
        assert_eq!(
            desktop_socket_path().unwrap(),
            &fig_util::directories::desktop_socket_path().unwrap()
        );
        assert_eq!(
            log_file_path().unwrap(),
            fig_util::directories::logs_dir().unwrap().join(LOG_FILE_NAME)
        );
    }
}
