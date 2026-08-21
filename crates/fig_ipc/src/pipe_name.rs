use std::path::Path;

/// Map a desktop socket path onto a Windows named-pipe name.
/// Used only on Windows at runtime; compiled everywhere so the mapping is tested in macOS CI.
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
}
