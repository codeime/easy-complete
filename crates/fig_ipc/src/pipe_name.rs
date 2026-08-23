use std::path::Path;

/// Map a desktop socket path onto a Windows named-pipe name.
///
/// Used only on Windows at runtime; compiled everywhere so the mapping is
/// tested in macOS and Linux CI. Live accept/connect stays `cfg(windows)`
/// in `windows_pipe.rs`.
pub fn pipe_name_from_path(path: impl AsRef<Path>) -> String {
    let slug: String = path
        .as_ref()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(180)
        .collect();
    format!(r"\\.\pipe\ec_{slug}")
}

#[cfg(test)]
mod tests {
    use super::pipe_name_from_path;
    use std::path::Path;

    #[test]
    fn pipe_name_is_a_windows_pipe_and_stable() {
        let path = Path::new(r"C:\Users\me\AppData\Local\Temp\sockets\desktop.sock");
        let name = pipe_name_from_path(path);
        assert!(name.starts_with(r"\\.\pipe\ec_"));
        assert_eq!(name, pipe_name_from_path(path));
        assert!(name.len() < 256);
    }

    #[test]
    fn pipe_name_strips_spaces_and_stays_under_win32_limit() {
        let path = Path::new(r"C:\Users\Ada Lovelace\AppData\Local\Temp\easy-complete\sockets\desktop.sock");
        let name = pipe_name_from_path(path);
        assert!(name.starts_with(r"\\.\pipe\ec_"));
        assert!(!name.contains(' '));
        assert!(name.len() < 256, "{name} ({})", name.len());
    }

    #[test]
    fn pipe_name_truncates_before_the_win32_256_limit() {
        let path = "x".repeat(500);
        let name = pipe_name_from_path(path);
        assert!(name.starts_with(r"\\.\pipe\ec_"));
        assert_eq!(name.len(), r"\\.\pipe\ec_".len() + 180);
        assert!(name.len() < 256);
        assert!(!name.contains(':'));
    }

    #[test]
    fn empty_or_relative_path_still_stays_on_the_pipe_prefix() {
        let empty = pipe_name_from_path("");
        assert_eq!(empty, r"\\.\pipe\ec_");
        let relative = pipe_name_from_path("desktop.sock");
        assert!(relative.starts_with(r"\\.\pipe\ec_"));
        assert!(
            relative.ends_with("desktop_sock"),
            "'.' in the path must be slugged, got {relative}"
        );
        let already = pipe_name_from_path(r"\\.\pipe\already");
        assert!(already.starts_with(r"\\.\pipe\ec_"));
        assert!(
            !already[r"\\.\pipe\ec_".len()..].contains('\\'),
            "backslash after the prefix must be slugged, got {already}"
        );
    }
}
