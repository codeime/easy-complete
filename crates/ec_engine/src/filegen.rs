//! Filesystem path generator (IRIS FileGenerator).

use std::cmp::Ordering;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use fancy_regex::Regex;

use crate::query::matches_query;
use crate::runtime::Suggestion;

const MAX_RESULTS: usize = 50;

/// Fig `filepaths({ … })` options recovered at compile time. Empty is the
/// same as a bare `template: "filepaths"` / `"folders"`.
#[derive(Debug, Clone, Default)]
pub struct PathFilter<'a> {
    pub folders_only: bool,
    pub files_only: bool,
    pub extensions: &'a [String],
    pub equals: &'a [String],
    pub filter_folders: bool,
    pub file_priority: Option<i64>,
    pub folder_priority: Option<i64>,
    pub root_directory: Option<&'a str>,
    pub environment: &'a [(String, String)],
    pub matches: Option<&'a str>,
    pub matches_flags: Option<&'a str>,
}

#[cfg(test)]
pub fn complete_path(prefix: &str, cwd: &str, folders_only: bool, fuzzy: bool) -> Vec<Suggestion> {
    complete_path_filtered(
        prefix,
        cwd,
        fuzzy,
        &PathFilter {
            folders_only,
            ..PathFilter::default()
        },
    )
}

pub fn complete_path_filtered(prefix: &str, cwd: &str, fuzzy: bool, filter: &PathFilter<'_>) -> Vec<Suggestion> {
    let list_cwd = filter.root_directory.filter(|root| !root.is_empty()).unwrap_or(cwd);
    if list_cwd.is_empty() && !prefix.starts_with('/') && !prefix.starts_with('~') {
        return Vec::new();
    }
    if prefix == "~" {
        return complete_path_filtered("~/", cwd, fuzzy, filter);
    }
    let expanded = expand_home_with(prefix, filter.environment);
    let (dir, query) = split_prefix(&expanded, list_cwd);
    let display_prefix = dir_prefix(prefix);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let matches = compiled_matches(filter);
    let mut names = Vec::new();
    for entry in entries.flatten() {
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
        let is_dir = entry.path().is_dir();
        if filter.folders_only && !is_dir {
            continue;
        }
        if filter.files_only && is_dir {
            continue;
        }
        if is_dir && !name.ends_with('/') {
            name.push('/');
        }
        if !path_name_passes_filter(&name, is_dir, filter, matches.as_ref()) {
            continue;
        }
        if !query.is_empty() && !matches_query(&name, &query, fuzzy) {
            continue;
        }
        names.push((name, is_dir));
    }

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
            Ordering::Equal => compare_file_names(&left.0, &right.0),
            order => order,
        }
    });

    let mut suggestions = names
        .into_iter()
        .take(MAX_RESULTS.saturating_sub(1))
        .map(|(name, is_dir)| {
            let name = format!("{display_prefix}{name}");
            path_suggestion_with_filter(name, is_dir, filter)
        })
        .collect::<Vec<_>>();

    // `sortFilesAlphabetically` appends this entry even when the directory is
    // empty.  Keep the row itself as `../`, like the old generator.  Its
    // query-term is the final path segment: accepting it from `src/.` then
    // deletes only `.` and leaves the already-entered `src/` in place.
    if !filter.files_only
        && matches_query("../", &query, fuzzy)
        && path_name_passes_filter("../", true, filter, matches.as_ref())
    {
        suggestions.push(path_suggestion_with_filter("../".into(), true, filter).with_query_term(Some(query)));
    }
    suggestions
}

fn path_name_passes_filter(name: &str, is_dir: bool, filter: &PathFilter<'_>, matches: Option<&Regex>) -> bool {
    if filter.extensions.is_empty() && filter.equals.is_empty() && filter.matches.is_none() {
        return true;
    }
    if is_dir && !filter.filter_folders {
        return true;
    }
    if filter.equals.iter().any(|allowed| allowed == name) {
        return true;
    }
    if matches.is_some_and(|regex| regex.is_match(name).unwrap_or(false)) {
        return true;
    }
    extension_matches(name, filter.extensions)
}

/// Fig `matches` is a JavaScript `RegExp` source. `fancy-regex` keeps
/// lookarounds such as direnv's `/\.env(?!rc)/` instead of dropping the filter.
fn compiled_matches(filter: &PathFilter<'_>) -> Option<Regex> {
    let source = filter.matches.filter(|source| !source.is_empty())?;
    let mut prefix = String::new();
    if let Some(flags) = filter.matches_flags {
        if flags.contains('i') {
            prefix.push('i');
        }
        if flags.contains('m') {
            prefix.push('m');
        }
        if flags.contains('s') {
            prefix.push('s');
        }
    }
    let pattern = if prefix.is_empty() {
        source.to_string()
    } else {
        format!("(?{prefix}){source}")
    };
    Regex::new(&pattern).ok()
}

/// Fig `filepaths` matches `extensions` against successive suffixes of the
/// name after the first dot (`foo.bar.py` → `py`, then `bar.py`).
fn extension_matches(name: &str, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return false;
    }
    let mut parts = name.split('.');
    let Some(_) = parts.next() else {
        return false;
    };
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() {
        return false;
    }
    let mut suffix = rest[rest.len() - 1].to_string();
    let mut index = rest.len() - 1;
    loop {
        if extensions.iter().any(|extension| extension == &suffix) {
            return true;
        }
        if index == 0 {
            return false;
        }
        index -= 1;
        suffix = format!("{}.{}", rest[index], suffix);
    }
}

fn path_suggestion_with_filter(name: String, is_dir: bool, filter: &PathFilter<'_>) -> Suggestion {
    let kind = if is_dir { "folder" } else { "file" };
    // The WebView leaves description empty and Description.tsx falls back to
    // the lowercase suggestion type.  Keeping it explicit makes the native
    // row independent of that UI fallback while preserving the visible text.
    let suggestion = Suggestion::new(name.clone(), kind, kind).with_insert_value(name);
    let priority = if is_dir {
        filter.folder_priority
    } else {
        filter.file_priority
    };
    match priority {
        Some(priority) => suggestion.with_priority(priority),
        None => suggestion,
    }
}

/// Approximate JavaScript's default `localeCompare` for the ASCII-heavy shell
/// names produced by `ls`, while making ties deterministic across filesystems.
/// Lowercase sorts before uppercase within an otherwise equal spelling (the
/// ordering used by Node's default English locale in the old implementation).
fn compare_file_names(left: &str, right: &str) -> Ordering {
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

#[cfg(test)]
pub fn expand_home(path: &str) -> String {
    expand_home_with(path, &[])
}

fn expand_home_with(path: &str, env: &[(String, String)]) -> String {
    expand_environment_variables(&expand_tilde(path, env), env)
}

/// Expand the same shell-prefix forms that the legacy `shellExpand` helper
/// supports, without invoking a shell.  The returned path is only used for
/// filesystem lookup; callers keep the original prefix for insertion text.
fn expand_tilde(path: &str, env: &[(String, String)]) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };

    // `~` and `~/...` use the shell HOME when bound, then the process HOME.
    if rest.is_empty() || rest.starts_with('/') {
        let home = environment_value("HOME", env)
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        // Strip all separators before joining.  `Path::join` treats an
        // absolute suffix as a replacement path, whereas shell tilde
        // expansion keeps the user's separators after the home directory
        // (`~//tmp` is still rooted under HOME).
        let suffix = rest.trim_start_matches('/');
        return home.map_or_else(
            || path.to_string(),
            |home| {
                join_home_prefix(
                    &home,
                    suffix,
                    rest.is_empty() || (rest.starts_with('/') && suffix.is_empty()),
                )
            },
        );
    }

    // Resolve `~user` through the local account database.  This is a
    // read-only lookup and never executes shell code.  Unknown users remain
    // untouched, so a literal path is not accidentally redirected.
    let username_end = rest.find('/').unwrap_or(rest.len());
    let username = &rest[..username_end];
    let Some(home) = user_home_dir(username) else {
        return path.to_string();
    };
    let suffix = &rest[username_end..];
    join_home_prefix(
        &home,
        suffix.trim_start_matches('/'),
        suffix.is_empty() || (suffix.starts_with('/') && suffix.trim_start_matches('/').is_empty()),
    )
}

fn join_home_prefix(home: &Path, suffix: &str, append_slash: bool) -> String {
    let mut expanded = if suffix.is_empty() {
        home.display().to_string()
    } else {
        home.join(suffix).display().to_string()
    };
    if append_slash && !expanded.ends_with('/') {
        expanded.push('/');
    }
    expanded
}

/// Expand `$NAME`, `${NAME}`, and `${NAME:-fallback}` using the process
/// environment.  Unknown variables and malformed expressions are copied
/// literally, matching the old helper's nullish fallback behavior while
/// avoiding shell evaluation and command substitution.
fn expand_environment_variables(path: &str, env: &[(String, String)]) -> String {
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
                if let Some(value) = environment_value(name, env) {
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
        if let Some(value) = environment_value(&name, env) {
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

fn environment_value(name: &str, env: &[(String, String)]) -> Option<String> {
    if let Some((_, value)) = env.iter().find(|(key, _)| key == name) {
        return Some(value.clone());
    }
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

#[cfg(not(unix))]
fn user_home_dir(_username: &str) -> Option<PathBuf> {
    None
}

fn dir_prefix(prefix: &str) -> String {
    if prefix.ends_with('/') {
        prefix.to_string()
    } else if let Some(slash) = prefix.rfind('/') {
        prefix[..=slash].to_string()
    } else {
        String::new()
    }
}

fn split_prefix(prefix: &str, cwd: &str) -> (PathBuf, String) {
    let base = if cwd.is_empty() {
        if prefix.starts_with('/') || prefix.starts_with('~') {
            PathBuf::from("/")
        } else {
            return (PathBuf::new(), String::new());
        }
    } else {
        PathBuf::from(cwd)
    };
    if prefix.is_empty() {
        return (base, String::new());
    }
    if prefix.ends_with('/') {
        return (join_base(&base, prefix), String::new());
    }
    if let Some(slash) = prefix.rfind('/') {
        let dir = &prefix[..=slash];
        let query = prefix[slash + 1..].to_string();
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
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into());
        let expanded = expand_home("~/Desktop");
        assert!(
            expanded.starts_with(&home) && expanded.ends_with("Desktop"),
            "{expanded}"
        );
        let home_dir = expand_home("~");
        assert!(home_dir.ends_with('/'), "{home_dir}");
        assert_eq!(expand_home("~/"), home_dir);
    }

    #[test]
    fn matches_regex_keeps_env_files_and_unfiltered_folders() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "").unwrap();
        fs::write(dir.path().join(".env.foo"), "").unwrap();
        fs::write(dir.path().join("keep.py"), "").unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();
        let cwd = dir.path().display().to_string();
        let names: Vec<_> = complete_path_filtered(
            "",
            &cwd,
            false,
            &PathFilter {
                matches: Some(r"^\.env.*$"),
                ..PathFilter::default()
            },
        )
        .into_iter()
        .map(|row| row.name)
        .collect();
        assert!(names.contains(&".env".into()), "{names:?}");
        assert!(names.contains(&".env.foo".into()), "{names:?}");
        assert!(names.contains(&"folder/".into()), "{names:?}");
        assert!(names.contains(&"../".into()), "{names:?}");
        assert!(!names.contains(&"keep.py".into()), "{names:?}");
    }

    #[test]
    fn matches_javascript_negative_lookahead() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "").unwrap();
        fs::write(dir.path().join(".envrc"), "").unwrap();
        fs::write(dir.path().join(".env.local"), "").unwrap();
        let cwd = dir.path().display().to_string();
        let names: Vec<_> = complete_path_filtered(
            "",
            &cwd,
            false,
            &PathFilter {
                matches: Some(r"\.env(?!rc)"),
                matches_flags: Some("g"),
                ..PathFilter::default()
            },
        )
        .into_iter()
        .map(|row| row.name)
        .collect();
        assert!(names.contains(&".env".into()), "{names:?}");
        assert!(names.contains(&".env.local".into()), "{names:?}");
        assert!(!names.contains(&".envrc".into()), "{names:?}");
    }

    #[test]
    fn equals_keeps_named_file_and_unfiltered_folders() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        fs::write(dir.path().join("keep.py"), "").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        let cwd = dir.path().display().to_string();
        let equals = vec!["Cargo.toml".into()];
        let names: Vec<_> = complete_path_filtered(
            "",
            &cwd,
            false,
            &PathFilter {
                equals: &equals,
                ..PathFilter::default()
            },
        )
        .into_iter()
        .map(|row| row.name)
        .collect();
        assert!(names.contains(&"Cargo.toml".into()), "{names:?}");
        assert!(names.contains(&"src/".into()), "{names:?}");
        assert!(!names.contains(&"keep.py".into()), "{names:?}");
    }

    #[test]
    fn filter_folders_drops_unrelated_directories_and_parent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep.py"), "").unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();
        let cwd = dir.path().display().to_string();
        let extensions = vec!["py".into()];
        let names: Vec<_> = complete_path_filtered(
            "",
            &cwd,
            false,
            &PathFilter {
                extensions: &extensions,
                filter_folders: true,
                ..PathFilter::default()
            },
        )
        .into_iter()
        .map(|row| row.name)
        .collect();
        assert!(names.contains(&"keep.py".into()), "{names:?}");
        assert!(!names.contains(&"folder/".into()), "{names:?}");
        assert!(!names.contains(&"../".into()), "{names:?}");
    }

    #[test]
    fn extensions_keep_matching_suffixes_and_unfiltered_folders() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep.py"), "").unwrap();
        fs::write(dir.path().join("drop.txt"), "").unwrap();
        fs::write(dir.path().join("foo.bar.py"), "").unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();
        let cwd = dir.path().display().to_string();
        let extensions = vec!["py".into()];
        let names: Vec<_> = complete_path_filtered(
            "",
            &cwd,
            false,
            &PathFilter {
                extensions: &extensions,
                ..PathFilter::default()
            },
        )
        .into_iter()
        .map(|row| row.name)
        .collect();
        assert!(names.contains(&"keep.py".into()), "{names:?}");
        assert!(names.contains(&"foo.bar.py".into()), "{names:?}");
        assert!(names.contains(&"folder/".into()), "{names:?}");
        assert!(!names.contains(&"drop.txt".into()), "{names:?}");
    }

    #[test]
    fn shell_environment_overrides_process_variables() {
        let key = "EC_FILEGEN_SHELL_ENV";
        let previous = std::env::var_os(key);
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var(key, "/from-process") };
        let env = vec![(key.to_string(), "/from-shell".into())];
        assert_eq!(expand_home_with(&format!("${key}/child"), &env), "/from-shell/child");
        assert_eq!(expand_home(&format!("${key}/child")), "/from-process/child");
        unsafe {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
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
}
