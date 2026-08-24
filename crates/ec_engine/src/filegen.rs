//! Filesystem path generator (IRIS FileGenerator).

use std::cmp::Ordering;
use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use crate::query::matches_query;
use crate::runtime::Suggestion;

const MAX_RESULTS: usize = 50;
/// Same ceiling as the git-refs / package.json mtime caches. A keystroke
/// in `src/` must not `read_dir` again when `src/m` follows; wholesale
/// clear at the cap is enough because listings are cheap to rebuild.
const MAX_CACHED_DIRS: usize = 32;

struct DirListing {
    mtime: Option<SystemTime>,
    entries: Arc<[(String, bool)]>,
}

static DIR_CACHE: LazyLock<Mutex<HashMap<PathBuf, DirListing>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn complete_path(prefix: &str, cwd: &str, folders_only: bool, fuzzy: bool) -> Vec<Suggestion> {
    // `~\` is only a home reference where a backslash separates paths. On unix
    // it is a literal filename, and `~\D` already resolves that way, so
    // redirecting the bare form here would split one prefix family in two.
    if prefix == "~" || (cfg!(not(unix)) && prefix == "~\\") {
        return complete_path("~/", cwd, folders_only, fuzzy);
    }
    let expanded = expand_home(prefix);
    let (dir, query) = split_prefix(&expanded, cwd);
    let display_prefix = dir_prefix(prefix);
    let Some(entries) = cached_directory_listing(&dir) else {
        return Vec::new();
    };
    // `folders_only` and the query filter run on the shared listing so
    // `ls ` and `cd ` (and `src/m` / `src/ma`) reuse one directory snapshot.
    let mut names: Vec<(&str, bool)> = entries
        .iter()
        .filter(|(name, is_dir)| {
            if folders_only && !is_dir {
                return false;
            }
            query.is_empty() || matches_query(name, &query, fuzzy)
        })
        .map(|(name, is_dir)| (name.as_str(), *is_dir))
        .collect();

    // This mirrors @fig/autocomplete-generators' sortFilesAlphabetically:
    // ordinary entries come first, dotfiles follow them, and the synthetic
    // parent entry is always last.  The old generator deliberately lists
    // dotfiles even for an empty query (`ls -1ApL`), leaving query filtering
    // to the suggestion pipeline.  Filtering here is equivalent for the
    // native engine, but must not hide dotfiles before that matching step.
    names.sort_by(|left, right| {
        let left_hidden = left.0.starts_with('.');
        let right_hidden = right.0.starts_with('.');
        match left_hidden.cmp(&right_hidden) {
            Ordering::Equal => compare_file_names(left.0, right.0),
            order => order,
        }
    });

    let mut suggestions = names
        .into_iter()
        .take(MAX_RESULTS.saturating_sub(1))
        .map(|(name, is_dir)| {
            let name = format!("{display_prefix}{name}");
            path_suggestion(name, is_dir)
        })
        .collect::<Vec<_>>();

    // `sortFilesAlphabetically` appends this entry even when the directory is
    // empty.  Keep the row itself as `../`, like the old generator.  Its
    // query-term is the final path segment: accepting it from `src/.` then
    // deletes only `.` and leaves the already-entered `src/` in place.
    if matches_query("../", &query, fuzzy) {
        suggestions.push(path_suggestion("../".into(), true).with_query_term(Some(query)));
    }
    suggestions
}

/// Unfiltered directory rows, including trailing `/` on folders. Query and
/// `folders_only` stay with the caller so `ls` and `cd` share one listing.
fn cached_directory_listing(dir: &Path) -> Option<Arc<[(String, bool)]>> {
    let mtime = fs::metadata(dir).and_then(|meta| meta.modified()).ok();
    {
        let cache = DIR_CACHE.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(listing) = cache.get(dir)
            && listing.mtime == mtime
            && mtime.is_some()
        {
            return Some(Arc::clone(&listing.entries));
        }
    }
    let read = fs::read_dir(dir).ok()?;
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        // The legacy filepaths generator passes `.DS_Store` through its
        // default skip list (case-insensitive), even though other dotfiles
        // remain visible.
        if name.eq_ignore_ascii_case(".DS_Store") {
            continue;
        }
        let is_dir = entry_is_dir(&entry);
        if is_dir && !name.ends_with('/') {
            name.push('/');
        }
        entries.push((name, is_dir));
    }
    let entries: Arc<[(String, bool)]> = entries.into();
    if mtime.is_some() {
        let mut cache = DIR_CACHE.lock().unwrap_or_else(|err| err.into_inner());
        if cache.len() >= MAX_CACHED_DIRS && !cache.contains_key(dir) {
            cache.clear();
        }
        cache.insert(
            dir.to_path_buf(),
            DirListing {
                mtime,
                entries: Arc::clone(&entries),
            },
        );
    }
    Some(entries)
}

fn path_suggestion(name: String, is_dir: bool) -> Suggestion {
    let kind = if is_dir { "folder" } else { "file" };
    // The WebView leaves description empty and Description.tsx falls back to
    // the lowercase suggestion type.  Keeping it explicit makes the native
    // row independent of that UI fallback while preserving the visible text.
    Suggestion::new(name.clone(), kind, kind).with_insert_value(name)
}

/// Follow directory entries the way `Path::is_dir` does, but skip a `stat` for
/// ordinary files and directories when `DirEntry::file_type` already knows.
/// Symlinks still go through `path().is_dir()` so `link/` → `src/` stays a folder.
fn entry_is_dir(entry: &fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(file_type) if file_type.is_dir() => true,
        Ok(file_type) if file_type.is_symlink() => entry.path().is_dir(),
        Ok(_) => false,
        Err(_) => entry.path().is_dir(),
    }
}

/// Approximate JavaScript's default `localeCompare` for the ASCII-heavy shell
/// names produced by `ls`, while making ties deterministic across filesystems.
/// Lowercase sorts before uppercase within an otherwise equal spelling (the
/// ordering used by Node's default English locale in the old implementation).
fn compare_file_names(left: &str, right: &str) -> Ordering {
    // Same fold as concatenating `ch.to_lowercase()` per scalar, without a
    // String per character on every comparison in the directory sort.
    let mut left_fold = left.chars().flat_map(char::to_lowercase);
    let mut right_fold = right.chars().flat_map(char::to_lowercase);
    let folded = loop {
        match (left_fold.next(), right_fold.next()) {
            (Some(a), Some(b)) => {
                let order = a.cmp(&b);
                if order != Ordering::Equal {
                    break order;
                }
            },
            (None, Some(_)) => break Ordering::Less,
            (Some(_), None) => break Ordering::Greater,
            (None, None) => break Ordering::Equal,
        }
    };
    if folded != Ordering::Equal {
        return folded;
    }

    left.chars()
        .zip(right.chars())
        .find_map(|(left, right)| {
            if left == right {
                None
            } else if left.is_lowercase() != right.is_lowercase() {
                Some(if left.is_lowercase() {
                    Ordering::Less
                } else {
                    Ordering::Greater
                })
            } else {
                Some(left.cmp(&right))
            }
        })
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

pub fn expand_home(path: &str) -> String {
    expand_environment_variables(&expand_tilde(path))
}

/// Expand the same shell-prefix forms that the legacy `shellExpand` helper
/// supports, without invoking a shell.  The returned path is only used for
/// filesystem lookup; callers keep the original prefix for insertion text.
fn expand_tilde(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };

    // `~` and `~/...` use the current process HOME, preserving the existing
    // behavior when HOME is unavailable.
    if rest.is_empty() || starts_with_path_sep(rest) {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        // Strip all separators before joining.  `Path::join` treats an
        // absolute suffix as a replacement path, whereas shell tilde
        // expansion keeps the user's separators after the home directory
        // (`~//tmp` is still rooted under HOME).
        let suffix = rest.trim_start_matches(PATH_SEPS);
        return home.map_or_else(
            || path.to_string(),
            |home| {
                join_home_prefix(
                    &home,
                    suffix,
                    rest.is_empty() || (starts_with_path_sep(rest) && suffix.is_empty()),
                )
            },
        );
    }

    // Resolve `~user` through the local account database.  This is a
    // read-only lookup and never executes shell code.  Unknown users remain
    // untouched, so a literal path is not accidentally redirected.
    let username_end = rest.find(PATH_SEPS).unwrap_or(rest.len());
    let username = &rest[..username_end];
    let Some(home) = user_home_dir(username) else {
        return path.to_string();
    };
    let suffix = rest[username_end..].trim_start_matches(PATH_SEPS);
    join_home_prefix(
        &home,
        suffix,
        rest[username_end..].is_empty() || (starts_with_path_sep(&rest[username_end..]) && suffix.is_empty()),
    )
}

fn starts_with_path_sep(s: &str) -> bool {
    s.starts_with(PATH_SEPS)
}

fn join_home_prefix(home: &Path, suffix: &str, append_slash: bool) -> String {
    let mut path = home.to_path_buf();
    for part in suffix.split(PATH_SEPS).filter(|part| !part.is_empty()) {
        path.push(part);
    }
    let mut expanded = path.display().to_string();
    if append_slash && !ends_with_path_sep(&expanded) {
        expanded.push(std::path::MAIN_SEPARATOR);
    }
    expanded
}

/// Expand `$NAME`, `${NAME}`, and `${NAME:-fallback}` using the process
/// environment.  Unknown variables and malformed expressions are copied
/// literally, matching the old helper's nullish fallback behavior while
/// avoiding shell evaluation and command substitution.
fn expand_environment_variables(path: &str) -> String {
    let chars: Vec<char> = path.chars().collect();
    let mut expanded = String::with_capacity(path.len());
    let mut index = 0;

    while index < chars.len() {
        if chars[index] != '$' {
            expanded.push(chars[index]);
            index += 1;
            continue;
        }

        if chars.get(index + 1) == Some(&'{') {
            let Some(relative_end) = chars[index + 2..].iter().position(|character| *character == '}') else {
                expanded.push('$');
                index += 1;
                continue;
            };
            let end = index + 2 + relative_end;
            let body: String = chars[index + 2..end].iter().collect();
            if let Some((name, fallback)) = parse_braced_variable(&body) {
                if let Some(value) = environment_value(name) {
                    expanded.push_str(&value);
                } else if let Some(fallback) = fallback {
                    expanded.push_str(fallback);
                } else {
                    expanded.extend(chars[index..=end].iter());
                }
                index = end + 1;
                continue;
            }
            expanded.extend(chars[index..=end].iter());
            index = end + 1;
            continue;
        }

        let name_start = index + 1;
        let mut name_end = name_start;
        while let Some(character) = chars.get(name_end) {
            if !is_environment_name_character(*character) {
                break;
            }
            name_end += 1;
        }
        if name_end == name_start {
            expanded.push('$');
            index += 1;
            continue;
        }

        let name: String = chars[name_start..name_end].iter().collect();
        if let Some(value) = environment_value(&name) {
            expanded.push_str(&value);
        } else {
            expanded.extend(chars[index..name_end].iter());
        }
        index = name_end;
    }

    expanded
}

fn parse_braced_variable(body: &str) -> Option<(&str, Option<&str>)> {
    let (name, fallback) = body
        .split_once(":-")
        .map_or((body, None), |(name, fallback)| (name, Some(fallback)));
    (!name.is_empty() && name.chars().all(is_environment_name_character)).then_some((name, fallback))
}

fn is_environment_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn environment_value(name: &str) -> Option<String> {
    // `var` deliberately rejects non-Unicode values instead of lossy
    // conversion; an unrepresentable environment value is left literal.
    std::env::var(name).ok()
}

#[cfg(unix)]
fn user_home_dir(username: &str) -> Option<PathBuf> {
    let username = CString::new(username).ok()?;
    // `getpwnam` uses process-global storage and can race when several
    // completion requests resolve `~user` concurrently. Use the re-entrant
    // variant and keep its backing buffer alive until the path is copied.
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut capacity = usize::try_from(suggested)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(16 * 1024);
    capacity = capacity.clamp(1024, 1024 * 1024);

    loop {
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; capacity];
        let status = unsafe {
            libc::getpwnam_r(
                username.as_ptr(),
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && capacity < 1024 * 1024 {
            capacity = (capacity * 2).min(1024 * 1024);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let passwd = unsafe { passwd.assume_init() };
        if passwd.pw_dir.is_null() {
            return None;
        }
        return unsafe { CStr::from_ptr(passwd.pw_dir) }
            .to_str()
            .ok()
            .map(PathBuf::from);
    }
}

/// Windows has no `getpwnam`. Only the current account maps through
/// `%USERPROFILE%`; any other `~user` stays literal.
#[cfg(any(test, not(unix)))]
fn windows_current_user_home(
    username: &str,
    current_username: Option<&str>,
    userprofile: Option<PathBuf>,
) -> Option<PathBuf> {
    let current = current_username?;
    if !current.eq_ignore_ascii_case(username) {
        return None;
    }
    userprofile
}

#[cfg(not(unix))]
fn user_home_dir(username: &str) -> Option<PathBuf> {
    windows_current_user_home(
        username,
        std::env::var("USERNAME").ok().as_deref(),
        std::env::var_os("USERPROFILE").map(PathBuf::from),
    )
}

/// Path separators for the current platform.
///
/// A backslash is an ordinary, legal character in a Unix filename, and the
/// tokenizer hands us the unescaped form — so treating it as a separator here
/// would split `a\b` into the directory `a\` and lose the file entirely.
#[cfg(unix)]
pub(crate) const PATH_SEPS: &[char] = &['/'];
#[cfg(not(unix))]
pub(crate) const PATH_SEPS: &[char] = &['/', '\\'];

fn last_path_sep(s: &str) -> Option<usize> {
    s.rfind(PATH_SEPS)
}

fn ends_with_path_sep(s: &str) -> bool {
    s.ends_with(PATH_SEPS)
}

fn dir_prefix(prefix: &str) -> String {
    if ends_with_path_sep(prefix) {
        prefix.to_string()
    } else if let Some(sep) = last_path_sep(prefix) {
        prefix[..=sep].to_string()
    } else {
        String::new()
    }
}

fn split_prefix(prefix: &str, cwd: &str) -> (PathBuf, String) {
    let base = if cwd.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(cwd)
    };
    if prefix.is_empty() {
        return (base, String::new());
    }
    if ends_with_path_sep(prefix) {
        return (join_base(&base, prefix), String::new());
    }
    if let Some(sep) = last_path_sep(prefix) {
        let dir = &prefix[..=sep];
        let query = prefix[sep + 1..].to_string();
        (join_base(&base, dir), query)
    } else {
        (base, prefix.to_string())
    }
}

fn join_base(base: &Path, rel: &str) -> PathBuf {
    let path = Path::new(rel);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_file_and_folder_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("readme.md"), "hi").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let cwd = dir.path().display().to_string();
        let files = complete_path("re", &cwd, false, false);
        assert!(files.iter().any(|s| s.name.starts_with("readme")), "{files:?}");
        let folders = complete_path("s", &cwd, true, false);
        assert!(folders.iter().any(|s| s.name == "src/"), "{folders:?}");
        assert!(folders.iter().all(|s| s.kind == "folder"));
    }

    #[test]
    fn preserves_directory_prefix_in_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        let cwd = dir.path().display().to_string();
        let files = complete_path("src/m", &cwd, false, false);
        assert!(files.iter().any(|s| s.name == "src/main.rs"), "{files:?}");
    }

    #[test]
    fn includes_dotfiles_and_parent_like_legacy_generator() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), "x").unwrap();
        fs::write(dir.path().join(".DS_Store"), "x").unwrap();
        fs::write(dir.path().join("visible"), "y").unwrap();
        let cwd = dir.path().display().to_string();
        let all = complete_path("", &cwd, false, false);
        let names: Vec<_> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["visible", ".hidden", "../"]);
        assert_eq!(all[0].description, "file");
        assert_eq!(all[2].description, "folder");

        let dotted = complete_path(".", &cwd, false, false);
        assert!(dotted.iter().any(|s| s.name == ".hidden"), "{dotted:?}");
        let parent = dotted.iter().find(|s| s.name == "../").expect("parent");
        assert_eq!(parent.query_term.as_deref(), Some("."));
        assert!(dotted.iter().all(|s| s.name.starts_with('.')), "{dotted:?}");
    }

    #[test]
    fn parent_keeps_typed_directory_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let cwd = dir.path().display().to_string();
        let suggestions = complete_path("src/", &cwd, false, false);
        let parent = suggestions.iter().find(|s| s.name == "../").expect("parent");
        assert_eq!(parent.query_term.as_deref(), Some(""));
        assert_eq!(parent.insert_value.as_deref(), Some("../"));
    }

    #[test]
    fn path_rows_keep_raw_insert_value_for_shell_escaping() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("my file's.txt"), "x").unwrap();
        let cwd = dir.path().display().to_string();
        let row = complete_path("my", &cwd, false, false)
            .into_iter()
            .find(|s| s.name == "my file's.txt")
            .expect("path row");
        // The old generator emits the raw name.  The insertion layer, which
        // intentionally ignores insertValue for file/folder rows, is then
        // responsible for shell quoting (`my\\ file'\\''s.txt`).
        assert_eq!(row.insert_value.as_deref(), Some("my file's.txt"));
        assert_eq!(row.description, "file");
    }

    #[test]
    fn sorts_regular_entries_before_dotfiles_and_parent() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["zeta", "Alpha", ".zeta", ".Alpha", "beta"] {
            fs::write(dir.path().join(name), "x").unwrap();
        }
        let cwd = dir.path().display().to_string();
        let names: Vec<_> = complete_path("", &cwd, false, false)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["Alpha", "beta", "zeta", ".Alpha", ".zeta", "../"]);
    }

    #[test]
    fn compare_file_names_matches_concatenated_to_lowercase() {
        fn allocating(left: &str, right: &str) -> Ordering {
            let folded = left
                .chars()
                .map(|ch| ch.to_lowercase().collect::<String>())
                .collect::<String>()
                .cmp(
                    &right
                        .chars()
                        .map(|ch| ch.to_lowercase().collect::<String>())
                        .collect::<String>(),
                );
            if folded != Ordering::Equal {
                return folded;
            }
            left.chars()
                .zip(right.chars())
                .find_map(|(left, right)| {
                    if left == right {
                        None
                    } else if left.is_lowercase() != right.is_lowercase() {
                        Some(if left.is_lowercase() {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        })
                    } else {
                        Some(left.cmp(&right))
                    }
                })
                .unwrap_or_else(|| left.len().cmp(&right.len()))
        }

        for (left, right) in [
            ("Alpha", "beta"),
            ("file", "File"),
            ("File", "file"),
            ("İ", "i"),
            ("ß", "ss"),
            ("相対", "absolute"),
            ("zeta", ".zeta"),
        ] {
            assert_eq!(
                compare_file_names(left, right),
                allocating(left, right),
                "{left:?} vs {right:?}"
            );
            assert_eq!(
                compare_file_names(right, left),
                allocating(right, left),
                "{right:?} vs {left:?}"
            );
        }
        assert_eq!(compare_file_names("file", "File"), Ordering::Less);
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlink_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("src"), dir.path().join("link")).unwrap();
        let cwd = dir.path().display().to_string();
        let folders = complete_path("l", &cwd, true, false);
        assert!(folders.iter().any(|s| s.name == "link/"), "{folders:?}");
    }

    #[test]
    fn preserves_tilde_prefix() {
        assert_eq!(dir_prefix("~/Des"), "~/");
        assert_eq!(dir_prefix("~"), "");
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/Users/test".into());
        let expanded = expand_home("~/Desktop");
        assert!(
            expanded.starts_with(&home) && expanded.ends_with("Desktop"),
            "{expanded}"
        );
        let home_dir = expand_home("~");
        assert!(home_dir.ends_with('/'), "{home_dir}");
        assert_eq!(expand_home("~/"), home_dir);
    }

    #[cfg(not(unix))]
    #[test]
    fn treats_backslash_as_a_separator_off_unix() {
        assert_eq!(dir_prefix(r"~\Des"), r"~\");
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| "/Users/test".into());
        let expanded_win = expand_home(r"~\Documents");
        assert!(
            expanded_win.starts_with(&home) && expanded_win.ends_with("Documents"),
            "{expanded_win}"
        );
        let (_, query) = split_prefix(r"C:\Users\me\Desktop", ".");
        assert_eq!(query, "Desktop");
        let (_, empty) = split_prefix(r"C:\Users\me\", ".");
        assert!(empty.is_empty(), "{empty}");
    }

    #[cfg(unix)]
    #[test]
    fn a_literal_backslash_is_part_of_a_unix_filename() {
        // The tokenizer unescapes before we see it, so `'a\b'` arrives as the
        // single token `a\b`. Splitting on the backslash would look for a
        // directory named `a\` and return nothing at all.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(r"we\ird"), "x").unwrap();
        let cwd = dir.path().display().to_string();

        let (base, query) = split_prefix(r"we\ir", &cwd);
        assert_eq!(base, dir.path(), "no directory component to split off");
        assert_eq!(query, r"we\ir");
        assert_eq!(dir_prefix(r"we\ir"), "");

        let names: Vec<_> = complete_path(r"we\ir", &cwd, false, false)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.iter().any(|name| name == r"we\ird"), "{names:?}");
    }

    #[test]
    fn expands_home_environment_prefixes() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        assert_eq!(expand_home("$HOME"), home);
        assert_eq!(expand_home("${HOME}/"), format!("{home}/"));
    }

    #[test]
    fn expands_simple_and_braced_environment_variables() {
        let key = "EC_FILEGEN_EXPANSION_TEST";
        let previous = std::env::var_os(key);
        // Environment mutation is process-global; serialize this test's
        // short critical section and restore the previous value below.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var(key, "/tmp/easy-complete-expansion") };

        assert_eq!(
            expand_home(&format!("${key}/child")),
            "/tmp/easy-complete-expansion/child"
        );
        assert_eq!(
            expand_home(&format!("${{{key}}}/child")),
            "/tmp/easy-complete-expansion/child"
        );
        assert_eq!(expand_home("${EC_FILEGEN_MISSING:-fallback}/child"), "fallback/child");

        unsafe {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn leaves_unknown_or_malformed_variables_literal() {
        assert_eq!(
            expand_home("$EC_FILEGEN_UNKNOWN_LITERAL/child"),
            "$EC_FILEGEN_UNKNOWN_LITERAL/child"
        );
        assert_eq!(
            expand_home("${EC_FILEGEN_UNKNOWN_LITERAL}/child"),
            "${EC_FILEGEN_UNKNOWN_LITERAL}/child"
        );
        assert_eq!(
            expand_home("${EC_FILEGEN_UNKNOWN_LITERAL"),
            "${EC_FILEGEN_UNKNOWN_LITERAL"
        );
        assert_eq!(expand_home("$/child"), "$/child");
    }

    #[test]
    fn expands_environment_prefix_for_filesystem_lookup_but_preserves_insertion_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("猫目录")).unwrap();
        let key = "EC_FILEGEN_PREFIX_TEST";
        let previous = std::env::var_os(key);
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var(key, dir.path()) };

        let prefix = format!("${key}/猫");
        let suggestions = complete_path(&prefix, "/", true, false);
        assert!(
            suggestions
                .iter()
                .any(|suggestion| suggestion.name == format!("{prefix}目录/")),
            "{suggestions:?}"
        );

        unsafe {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn handles_absolute_relative_and_unicode_queries() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("相対")).unwrap();
        fs::create_dir(dir.path().join("absolute")).unwrap();
        let cwd = dir.path().display().to_string();

        let relative = complete_path("相", &cwd, true, false);
        assert!(
            relative.iter().any(|suggestion| suggestion.name == "相対/"),
            "{relative:?}"
        );

        let absolute_prefix = format!("{cwd}/ab");
        let absolute = complete_path(&absolute_prefix, "/", true, false);
        assert!(
            absolute
                .iter()
                .any(|suggestion| suggestion.name == format!("{cwd}/absolute/")),
            "{absolute:?}"
        );
    }

    #[test]
    fn windows_tilde_user_only_matches_the_current_account() {
        let home = PathBuf::from(r"C:\Users\Ada");
        assert_eq!(
            windows_current_user_home("Ada", Some("ada"), Some(home.clone())),
            Some(home.clone())
        );
        assert_eq!(windows_current_user_home("bob", Some("ada"), Some(home.clone())), None);
        assert_eq!(windows_current_user_home("ada", None, Some(home)), None);
    }

    #[cfg(unix)]
    #[test]
    fn expands_existing_local_user_home_and_leaves_unknown_user_literal() {
        let Some(root_home) = user_home_dir("root") else {
            return;
        };
        let expected = format!("{}/", root_home.display());
        assert_eq!(expand_home("~root"), expected);
        assert_eq!(expand_home("~root/"), expected);
        assert_eq!(expand_home("~root/Desktop"), format!("{}/Desktop", root_home.display()));
        assert_eq!(
            expand_home("~ec-filegen-no-such-user-9/"),
            "~ec-filegen-no-such-user-9/"
        );
    }

    #[test]
    fn complete_path_reuses_a_mtime_listing_and_filters_after_the_cache() {
        let src = include_str!("filegen.rs");
        let start = src.find("pub fn complete_path").expect("complete_path");
        let listing = src.find("fn cached_directory_listing").expect("listing");
        let complete = &src[start..listing];
        assert!(
            complete.contains("cached_directory_listing")
                && !complete.contains("fs::read_dir")
                && complete.contains("folders_only"),
            "complete_path must filter folders_only/query on a cached listing, not readdir"
        );
        let end = src[listing..].find("fn path_suggestion").expect("path_suggestion") + listing;
        let body = &src[listing..end];
        assert!(
            body.contains("fs::read_dir") && body.contains("modified()") && body.contains("MAX_CACHED_DIRS"),
            "the listing cache must key on directory mtime and cap entries"
        );
        assert!(
            body.contains("mtime.is_some()") && body.contains("listing.mtime == mtime"),
            "a missing mtime is a miss, not a forever-empty cache"
        );
    }

    #[test]
    fn directory_listing_cache_invalidates_when_the_directory_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha"), "a").unwrap();
        let cwd = dir.path().display().to_string();
        let first = complete_path("", &cwd, false, false);
        assert!(first.iter().any(|s| s.name == "alpha"), "{first:?}");
        assert!(!first.iter().any(|s| s.name == "beta"), "{first:?}");

        fs::write(dir.path().join("beta"), "b").unwrap();
        if let Ok(mtime) = fs::metadata(dir.path()).and_then(|meta| meta.modified()) {
            let later = mtime + std::time::Duration::from_secs(1);
            let _ = fs::File::open(dir.path()).and_then(|file| file.set_modified(later));
        }
        let second = complete_path("b", &cwd, false, false);
        assert!(second.iter().any(|s| s.name == "beta"), "{second:?}");
    }
}
