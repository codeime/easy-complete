use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use fig_util::directories;

pub fn backup_file(path: impl AsRef<Path>, backup_dir: Option<impl Into<PathBuf>>) -> io::Result<()> {
    let pathref = path.as_ref();
    if pathref.exists() {
        let Some(name) = pathref.file_name() else {
            return Err(io::Error::new(ErrorKind::InvalidInput, "backup path has no file name"));
        };
        let dir = match backup_dir {
            Some(dir) => dir.into(),
            None => directories::utc_backup_dir().map_err(io::Error::other)?,
        };
        std::fs::create_dir_all(&dir)?;
        std::fs::copy(pathref, dir.join(name))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_file_does_not_unwrap() {
        let production = include_str!("backup.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            !production.contains(".unwrap()"),
            "a missing HOME or a path with no file name must return io::Error, not panic integrations install"
        );
        assert!(
            production.contains("utc_backup_dir()") && production.contains("file_name()"),
            "backup still copies into utc_backup_dir by default"
        );
    }

    #[test]
    fn backup_file_copies_into_the_backup_dir() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("dotfile");
        std::fs::write(&src, "hello").unwrap();
        backup_file(&src, Some(dst_dir.path())).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst_dir.path().join("dotfile")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn backup_file_skips_missing_paths() {
        backup_file("/nonexistent-easy-complete-backup-src", None::<PathBuf>).unwrap();
    }

    #[test]
    fn backup_path_without_file_name_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = Path::new("/");
        let result = backup_file(path, Some(dir.path()));
        if path.exists() && path.file_name().is_none() {
            assert!(result.is_err());
        }
    }
}
