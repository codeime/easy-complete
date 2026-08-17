//! Native generators: files, argv scripts, git refs, npm scripts/deps.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use crate::filegen;
use crate::ir::{ArgSpec, Builtin, Spec, Template};
use crate::process;
use crate::query::matches_query;
use crate::runtime::{Suggestion, query_term_for, suggestion_query_term};

fn hidden_static_seed_is_visible(seed: &crate::ir::SuggestionSeed, query: &str, kind: &str) -> bool {
    if !seed.meta.hidden {
        return true;
    }
    let exact_query = if kind == "folder" && !query.ends_with('/') {
        format!("{query}/")
    } else {
        query.to_string()
    };
    seed.names
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(&exact_query))
}

/// Fig's `getScriptSuggestions` supplies 5000ms when no user setting is
/// present.  Generator-specific `scriptTimeout` and an ExecuteCommand
/// object's `timeout` are then compared with that value using Math.max.
/// Keep the unit explicit here: settings and spec IR store milliseconds.
pub const DEFAULT_SCRIPT_TIMEOUT_MS: i64 = 5_000;
const MAX_RESULTS: usize = 50;
const MAX_CACHED: usize = 32;

pub(crate) fn configured_script_timeout_ms() -> i64 {
    fig_settings::settings::get_int("autocomplete.scriptTimeout")
        .ok()
        .flatten()
        .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS)
}

/// Port the old JS timeout selection without allowing a negative value to
/// wrap when converting to Rust's unsigned Duration.  JS's timer receives
/// zero for a result <= 0, so this clamp is the only conversion clamp.
fn effective_script_timeout_ms(
    default_timeout_ms: i64,
    generator_timeout_ms: Option<i64>,
    command_timeout_ms: Option<i64>,
) -> u64 {
    let selected = default_timeout_ms
        .max(generator_timeout_ms.unwrap_or(0))
        .max(command_timeout_ms.unwrap_or(0));
    u64::try_from(selected).unwrap_or(0)
}

fn script_timeout_for(arg: &ArgSpec) -> Duration {
    Duration::from_millis(effective_script_timeout_ms(
        configured_script_timeout_ms(),
        arg.script_timeout_ms,
        None,
    ))
}

struct MtimeLru<T> {
    entries: HashMap<String, (Option<SystemTime>, T)>,
    order: VecDeque<String>,
}

impl<T> Default for MtimeLru<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<T: Clone> MtimeLru<T> {
    fn get(&mut self, key: &str, mtime: Option<SystemTime>) -> Option<T> {
        if let Some((stored, value)) = self.entries.get(key) {
            if *stored == mtime {
                let hit = value.clone();
                if let Some(pos) = self.order.iter().position(|existing| existing == key) {
                    if pos + 1 != self.order.len() {
                        let key = self.order.remove(pos).expect("index from position()");
                        self.order.push_back(key);
                    }
                }
                return Some(hit);
            }
        } else {
            return None;
        }
        self.entries.remove(key);
        if let Some(pos) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(pos);
        }
        None
    }

    fn insert(&mut self, key: String, mtime: Option<SystemTime>, value: T) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), (mtime, value));
            if let Some(pos) = self.order.iter().position(|existing| existing == &key) {
                if pos + 1 != self.order.len() {
                    let key = self.order.remove(pos).expect("index from position()");
                    self.order.push_back(key);
                }
            }
            return;
        }
        if self.entries.len() >= MAX_CACHED {
            while let Some(old) = self.order.pop_front() {
                if self.entries.remove(&old).is_some() {
                    break;
                }
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, (mtime, value));
    }
}

static GIT_REFS: LazyLock<Mutex<MtimeLru<Arc<[String]>>>> = LazyLock::new(|| Mutex::new(MtimeLru::default()));
static PACKAGE_JSON: LazyLock<Mutex<MtimeLru<Arc<serde_json::Value>>>> =
    LazyLock::new(|| Mutex::new(MtimeLru::default()));

/// Compatibility wrapper used by generator-focused tests and callers that do
/// not already have a resolved lookup context.  The completion path calls
/// [`generate_for_arg`] with exactly one active argument.
#[allow(dead_code)]
pub fn generate(spec: &Spec, tokens: &[String], query: &str, cwd: &str, fuzzy: bool) -> Vec<Suggestion> {
    if query.starts_with('-') {
        return Vec::new();
    }
    let ends_with_space = query.is_empty();
    let active = crate::lookup::resolve_context(Arc::new(spec.clone()), tokens, ends_with_space, query, query, None)
        .active_arg
        .or_else(|| {
            spec.args.first().map(|arg| crate::lookup::ActiveArg {
                arg: arg.clone(),
                query: query.to_string(),
                search_term: query.to_string(),
                exclusive: false,
            })
        });
    if let Some(active) = active {
        return generate_for_arg_with_search_term(&active.arg, tokens, &active.query, &active.search_term, cwd, fuzzy);
    }
    Vec::new()
}

#[allow(dead_code)]
pub(crate) fn generate_for_arg(
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    cwd: &str,
    fuzzy: bool,
) -> Vec<Suggestion> {
    generate_for_arg_with_search_term(arg, tokens, query, query, cwd, fuzzy)
}

/// Generate rows for an active argument while retaining the raw shell search
/// term.  String getQueryTerm rules are per suggestion, so filtering and the
/// query term carried by each row must be computed independently from the
/// global result match term.
pub(crate) fn generate_for_arg_with_search_term(
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    search_term: &str,
    cwd: &str,
    fuzzy: bool,
) -> Vec<Suggestion> {
    let query = arg.meta.get_query_term.as_deref().map_or_else(
        || query.to_string(),
        |separator| query_term_for(search_term, Some(separator)),
    );
    let timeout = script_timeout_for(arg);
    let mut out = generate_arg(arg, tokens, &query, search_term, cwd, fuzzy, timeout);
    // `isDangerous` on an argument is a safety boundary for every completion
    // source under that argument, including scripts, builtins, and filepath
    // templates.  Preserve a stricter seed-level flag as well.
    if arg.meta.is_dangerous {
        for suggestion in &mut out {
            suggestion.is_dangerous = true;
        }
    }
    // Native generators (paths, argv scripts, and builtins) do not originate
    // from a static seed, so carry an argument-level query term onto every
    // generated row.  A seed-specific rule takes precedence below.
    if let Some(separator) = arg.meta.get_query_term.as_deref() {
        let query_term = query_term_for(search_term, Some(separator));
        for suggestion in &mut out {
            if suggestion.query_term.is_none() {
                suggestion.query_term = Some(query_term.clone());
            }
        }
    }
    dedup_suggestions(&mut out);
    out.truncate(MAX_RESULTS);
    out
}

fn generate_arg(
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    search_term: &str,
    cwd: &str,
    fuzzy: bool,
    timeout: Duration,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    for seed in &arg.suggestions {
        let suggestion_kind = seed.meta.suggestion_type.as_deref().unwrap_or("arg");
        let (seed_query, seed_query_term) =
            suggestion_query_term(suggestion_kind, seed.meta.get_query_term.as_deref(), query, search_term);
        let name = seed
            .names
            .iter()
            .find(|candidate| matches_query(candidate, &seed_query, fuzzy))
            .or_else(|| {
                seed.meta
                    .display_name
                    .as_deref()
                    .filter(|display| matches_query(display, &seed_query, fuzzy))
                    .and_then(|_| seed.names.first())
            });
        if let Some(name) = name {
            // Hidden static rows remain available when the user typed one of
            // their aliases exactly, including a case-insensitive spelling.
            // Partial and empty searches must continue to hide them.
            if !hidden_static_seed_is_visible(seed, &seed_query, suggestion_kind) {
                continue;
            }
            let display_name = seed
                .meta
                .display_name
                .clone()
                .or_else(|| (seed.names.len() > 1).then(|| seed.names.join(", ")));
            out.push(
                Suggestion::new(name.clone(), seed.description.clone(), suggestion_kind)
                    .with_args_hint(seed.args_hint.clone())
                    .with_meta(
                        seed.meta.insert_value.clone(),
                        display_name,
                        seed.meta.separator_to_add.clone(),
                        seed.meta.should_add_space.unwrap_or(false),
                        seed.meta.hidden,
                        seed.meta.priority,
                        seed.meta.icon.clone(),
                    )
                    .with_primary_name(seed.names.first().cloned())
                    .with_dangerous(seed.meta.is_dangerous || arg.meta.is_dangerous)
                    .with_original_type(seed.meta.original_type.clone())
                    .with_query_term(seed_query_term),
            );
        }
    }
    let mut builtins = arg.builtins.clone();
    if let Some(builtin) = arg.builtin {
        builtins.push(builtin);
    }
    let mut seen_builtins = HashSet::new();
    for builtin in builtins {
        if !seen_builtins.insert(builtin) {
            continue;
        }
        out.extend(match builtin {
            Builtin::GitRefs => git_refs(query, cwd, fuzzy, timeout),
            Builtin::GitBranches => git_branches(query, cwd, fuzzy, timeout),
            Builtin::GitTags => git_tags(query, cwd, fuzzy, timeout),
            Builtin::GitCommits => git_commits(query, cwd, fuzzy, timeout),
            Builtin::GitRemotes => git_remotes(query, cwd, fuzzy, timeout),
            Builtin::GitChangedFiles => git_changed_files(query, cwd, fuzzy, timeout),
            Builtin::GitStashes => git_stashes(query, cwd, fuzzy, timeout),
            Builtin::GitAliases => git_aliases(query, cwd, fuzzy, timeout),
            Builtin::NpmScripts => npm_scripts(query, cwd, fuzzy, timeout),
            Builtin::NpmDeps => npm_deps(query, cwd, fuzzy, timeout),
            Builtin::Cobra => crate::cobra::complete(tokens, cwd, fuzzy),
        });
    }
    if let Some((host, scope_cwd)) = crate::js_host::current() {
        let cwd = if cwd.is_empty() { scope_cwd } else { cwd };
        let has_js = arg.js_script.is_some() || arg.js_post_process.is_some() || arg.js_custom.is_some();
        if has_js {
            out.extend(run_js_generators(host, arg, tokens, query, cwd, fuzzy, timeout));
        } else if !arg.script.is_empty() {
            out.extend(run_script(
                &arg.script,
                query,
                cwd,
                fuzzy,
                timeout,
                arg.split_on.as_deref(),
            ));
        }
    } else if !arg.script.is_empty() {
        out.extend(run_script(
            &arg.script,
            query,
            cwd,
            fuzzy,
            timeout,
            arg.split_on.as_deref(),
        ));
    }
    let folders_only = arg.templates.contains(&Template::Folders) && !arg.templates.contains(&Template::Filepaths);
    if !arg.templates.is_empty() {
        out.extend(filegen::complete_path(query, cwd, folders_only, fuzzy));
    }
    out
}

fn dedup_suggestions(suggestions: &mut Vec<Suggestion>) {
    let mut seen = HashSet::new();
    suggestions.retain(|suggestion| {
        seen.insert((
            suggestion.name.clone(),
            suggestion.kind.clone(),
            suggestion.insert_value.clone(),
        ))
    });
}

fn run_js_generators(
    host: &crate::js_host::JsHost,
    arg: &ArgSpec,
    tokens: &[String],
    query: &str,
    cwd: &str,
    fuzzy: bool,
    timeout: Duration,
) -> Vec<Suggestion> {
    let has_script_hook = !arg.script.is_empty() || arg.js_script.is_some() || arg.js_post_process.is_some();
    if cwd.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if has_script_hook {
        let raw = crate::js_host::cached_suggestions(host, arg, tokens, cwd, "script", || {
            run_script_or_post_process(host, arg, tokens, cwd, timeout)
        });
        out.extend(
            raw.into_iter()
                .filter(|suggestion| matches_query(&suggestion.name, query, fuzzy)),
        );
    }
    if let Some(hook_id) = arg.js_custom.as_deref() {
        let custom = crate::js_host::cached_suggestions(host, arg, tokens, cwd, "custom", || {
            host.custom(hook_id, tokens, cwd, query, timeout, arg.meta.is_dangerous)
                .unwrap_or_default()
        });
        out.extend(
            custom
                .into_iter()
                .filter(|suggestion| matches_query(&suggestion.name, query, fuzzy)),
        );
    }
    out
}

fn run_script_or_post_process(
    host: &crate::js_host::JsHost,
    arg: &ArgSpec,
    tokens: &[String],
    cwd: &str,
    timeout: Duration,
) -> Vec<Suggestion> {
    let command = if let Some(hook_id) = arg.js_script.as_deref() {
        let Some(script) = host.script_command(hook_id, tokens) else {
            return Vec::new();
        };
        let timeout = Duration::from_millis(effective_script_timeout_ms(
            i64::try_from(timeout.as_millis()).unwrap_or(DEFAULT_SCRIPT_TIMEOUT_MS),
            arg.script_timeout_ms,
            script.timeout_ms,
        ));
        (script.command, script.args, timeout)
    } else if let Some((command, args)) = arg.script.split_first() {
        (command.clone(), args.to_vec(), timeout)
    } else {
        return Vec::new();
    };
    let stdout = process::execute(&command.0, &command.1, cwd, command.2);
    // Fig's `getScriptSuggestions` branches on `splitOn` first and only falls
    // back to `postProcess`. Specs that declare both rely on that order, so
    // running the hook here would feed it output it never expects.
    if let Some(separator) = arg.split_on.as_deref() {
        return all_split(&stdout, separator);
    }
    if let Some(hook_id) = arg.js_post_process.as_deref() {
        return host
            .post_process(hook_id, &crate::js_host::clean_output(&stdout), tokens)
            .unwrap_or_default();
    }
    all_lines(&stdout)
}

fn run_script(
    script: &[String],
    query: &str,
    cwd: &str,
    fuzzy: bool,
    timeout: Duration,
    split_on: Option<&str>,
) -> Vec<Suggestion> {
    let Some((command, args)) = script.split_first() else {
        return Vec::new();
    };
    let stdout = process::execute(command, args, cwd, timeout);
    match split_on {
        Some(separator) => filter_split(&stdout, query, fuzzy, separator),
        None => filter_lines(&stdout, query, fuzzy),
    }
}

fn filter_lines(stdout: &str, query: &str, fuzzy: bool) -> Vec<Suggestion> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| matches_query(line, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|line| Suggestion::new(line, "", "arg").with_insert_value(line))
        .collect()
}

/// Fig's `getScriptSuggestions` trims stdout, then `split(splitOn)`, and
/// drops empty pieces. Keep that shape so comma/`\n` generators match.
fn filter_split(stdout: &str, query: &str, fuzzy: bool, split_on: &str) -> Vec<Suggestion> {
    if split_on.is_empty() {
        return filter_lines(stdout, query, fuzzy);
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(split_on)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter(|part| matches_query(part, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|part| Suggestion::new(part, "", "arg").with_insert_value(part))
        .collect()
}

fn all_lines(stdout: &str) -> Vec<Suggestion> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| Suggestion::new(line, "", "arg").with_insert_value(line))
        .collect()
}

fn all_split(stdout: &str, split_on: &str) -> Vec<Suggestion> {
    if split_on.is_empty() {
        return all_lines(stdout);
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split(split_on)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| Suggestion::new(part, "", "arg").with_insert_value(part))
        .collect()
}

fn git_refs(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let (key, mtime) = git_identity(cwd);
    if let Ok(mut cache) = GIT_REFS.lock() {
        if let Some(lines) = cache.get(&key, mtime) {
            return filter_cached_lines(&lines, query, fuzzy);
        }
    }
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "for-each-ref".into(),
            "--format=%(refname:short)".into(),
            "refs/heads".into(),
            "refs/tags".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    let lines: Arc<[String]> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>()
        .into();
    if let Ok(mut cache) = GIT_REFS.lock() {
        cache.insert(key, mtime, lines.clone());
    }
    filter_cached_lines(&lines, query, fuzzy)
}

fn git_branches(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "for-each-ref".into(),
            "--format=%(refname:short)%09%(symref)".into(),
            "refs/heads".into(),
            "refs/remotes".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    branch_lines_to_suggestions(&stdout, query, fuzzy)
}

fn branch_lines_to_suggestions(stdout: &str, query: &str, fuzzy: bool) -> Vec<Suggestion> {
    let mut names: Vec<_> = stdout
        .lines()
        .filter_map(|line| {
            let (name, symref) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            // `refs/remotes/origin/HEAD` is a symbolic convenience ref. Git
            // shortens it to just `origin`, which otherwise looks like a real
            // branch beside `origin/main`.
            (symref.trim().is_empty() && !name.is_empty() && matches_query(name, query, fuzzy))
                .then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "", "branch"))
        .collect()
}

fn git_tags(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "for-each-ref".into(),
            "--format=%(refname:short)".into(),
            "refs/tags".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    lines_to_suggestions(&stdout, query, fuzzy, "tag")
}

fn git_commits(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "log".into(),
            "--all".into(),
            "--pretty=format:%h%x09%s".into(),
            "-n".into(),
            "100".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    stdout
        .lines()
        .filter_map(|line| {
            let (name, description) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            if name.is_empty() || !matches_query(name, query, fuzzy) {
                return None;
            }
            Some(Suggestion::new(name, description.trim(), "commit"))
        })
        .take(MAX_RESULTS)
        .collect()
}

fn git_remotes(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute("git", &["remote".into()], cwd, timeout) else {
        return Vec::new();
    };
    lines_to_suggestions(&stdout, query, fuzzy, "remote")
}

fn git_changed_files(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "status".into(),
            "--short".into(),
            "--untracked-files=all".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for line in stdout.lines() {
        let Some(path) = line.get(3..).map(str::trim) else {
            continue;
        };
        let path = path.rsplit_once(" -> ").map_or(path, |(_, new)| new).trim_matches('"');
        if !path.is_empty() && matches_query(path, query, fuzzy) {
            names.push(path.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "Changed file", "file"))
        .collect()
}

fn git_stashes(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "stash".into(),
            "list".into(),
            "--pretty=format:%gd%x09%s".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };
    stdout
        .lines()
        .filter_map(|line| {
            let (name, description) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            if name.is_empty() || !matches_query(name, query, fuzzy) {
                return None;
            }
            Some(Suggestion::new(name, description.trim(), "stash"))
        })
        .take(MAX_RESULTS)
        .collect()
}

fn lines_to_suggestions(stdout: &str, query: &str, fuzzy: bool, kind: &str) -> Vec<Suggestion> {
    let mut names: Vec<_> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && matches_query(line, query, fuzzy))
        .map(ToOwned::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "", kind))
        .collect()
}

fn git_aliases(query: &str, cwd: &str, fuzzy: bool, timeout: Duration) -> Vec<Suggestion> {
    let Some(stdout) = process::try_execute(
        "git",
        &[
            "--no-optional-locks".into(),
            "config".into(),
            "--get-regexp".into(),
            "^alias.".into(),
        ],
        cwd,
        timeout,
    ) else {
        return Vec::new();
    };

    stdout
        .lines()
        .filter_map(parse_git_alias)
        .filter(|suggestion| matches_query(&suggestion.name, query, fuzzy))
        .take(MAX_RESULTS)
        .collect()
}

fn parse_git_alias(line: &str) -> Option<Suggestion> {
    let line = line.trim();
    let value = line.strip_prefix("alias.")?;
    let mut fields = value.splitn(2, char::is_whitespace);
    let name = fields.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let expansion = fields.next().unwrap_or("").trim();
    Some(Suggestion::new(name, format!("Alias for '{expansion}'"), "arg"))
}

fn filter_cached_lines(lines: &[String], query: &str, fuzzy: bool) -> Vec<Suggestion> {
    lines
        .iter()
        .filter(|line| matches_query(line, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|line| Suggestion::new(line.clone(), "", "arg"))
        .collect()
}

fn git_identity(cwd: &str) -> (String, Option<SystemTime>) {
    let Some(git) = find_git_dir(cwd) else {
        return (format!("nogit:{cwd}"), None);
    };
    (git.display().to_string(), git_refs_mtime(&git))
}

fn git_refs_mtime(git: &std::path::Path) -> Option<SystemTime> {
    let mut best: Option<SystemTime> = None;
    for rel in ["HEAD", "packed-refs", "refs/heads", "refs/tags"] {
        if let Ok(mtime) = fs::metadata(git.join(rel)).and_then(|meta| meta.modified()) {
            best = Some(best.map_or(mtime, |prev| prev.max(mtime)));
        }
    }
    best.or_else(|| fs::metadata(git).and_then(|meta| meta.modified()).ok())
}

fn find_git_dir(cwd: &str) -> Option<PathBuf> {
    let mut dir = PathBuf::from(if cwd.is_empty() { "." } else { cwd });
    for _ in 0..16 {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        if git.is_file() {
            return resolve_gitdir_file(&git).or(Some(git));
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn resolve_gitdir_file(file: &PathBuf) -> Option<PathBuf> {
    let text = fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let rest = line.trim().strip_prefix("gitdir:")?.trim();
        if rest.is_empty() {
            continue;
        }
        let path = PathBuf::from(rest);
        let resolved = if path.is_absolute() {
            path
        } else {
            file.parent()?.join(path)
        };
        if resolved.is_dir() {
            return Some(resolved);
        }
    }
    None
}

fn npm_scripts(query: &str, cwd: &str, fuzzy: bool, _timeout: Duration) -> Vec<Suggestion> {
    let Some(pkg) = read_package_json(cwd) else {
        return Vec::new();
    };
    let Some(scripts) = pkg.get("scripts").and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut suggestions: Vec<Suggestion> = scripts
        .iter()
        .filter(|(name, _)| matches_query(name, query, fuzzy))
        .map(|(name, cmd)| Suggestion::new(name.clone(), cmd.as_str().unwrap_or(""), "arg"))
        .collect();
    suggestions.sort_by(|a, b| a.name.cmp(&b.name));
    suggestions.truncate(MAX_RESULTS);
    suggestions
}

fn npm_deps(query: &str, cwd: &str, fuzzy: bool, _timeout: Duration) -> Vec<Suggestion> {
    let Some(pkg) = read_package_json(cwd) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(deps) = pkg.get(key).and_then(|value| value.as_object()) {
            names.extend(deps.keys().cloned());
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|name| matches_query(name, query, fuzzy))
        .take(MAX_RESULTS)
        .map(|name| Suggestion::new(name, "dependency", "arg"))
        .collect()
}

fn read_package_json(cwd: &str) -> Option<Arc<serde_json::Value>> {
    let mut dir = PathBuf::from(if cwd.is_empty() { "." } else { cwd });
    for _ in 0..8 {
        let candidate = dir.join("package.json");
        if candidate.is_file() {
            let key = candidate.display().to_string();
            let mtime = fs::metadata(&candidate).and_then(|meta| meta.modified()).ok();
            if let Ok(mut cache) = PACKAGE_JSON.lock() {
                if let Some(parsed) = cache.get(&key, mtime) {
                    return Some(parsed);
                }
            }
            let bytes = fs::read(&candidate).ok()?;
            let parsed: Arc<serde_json::Value> = Arc::new(serde_json::from_slice(&bytes).ok()?);
            if let Ok(mut cache) = PACKAGE_JSON.lock() {
                cache.insert(key, mtime, parsed.clone());
            }
            return Some(parsed);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ArgSpec, OptionSpec, Spec, SuggestionSeed, Template};
    use std::fs;

    #[test]
    fn script_generator_filters_prefix() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![ArgSpec {
                script: vec!["printf".into(), "alpha\nbeta\nalpaca\n".into()],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "al".into()], "al", "/", false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"alpaca"), "{names:?}");
        assert!(!names.contains(&"beta"), "{names:?}");
    }

    #[test]
    fn script_split_on_uses_the_compiler_separator() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![ArgSpec {
                script: vec!["printf".into(), "alpha,beta,alpaca".into()],
                split_on: Some(",".into()),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "al".into()], "al", "/", false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"alpaca"), "{names:?}");
        assert!(!names.contains(&"beta"), "{names:?}");
        assert!(!names.iter().any(|name| name.contains(',')), "{names:?}");
    }

    #[test]
    fn empty_split_on_falls_back_to_newlines_and_trims_cr() {
        assert_eq!(
            filter_split("alpha\r\nbeta\r\nalpaca\r\n", "al", false, "")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "alpaca"]
        );
        assert_eq!(
            filter_split("alpha\r\nbeta\r\nalpaca\r\n", "al", false, "\n")
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "alpaca"]
        );
    }

    #[test]
    fn script_timeout_matches_webview_default_and_max_priority() {
        assert_eq!(DEFAULT_SCRIPT_TIMEOUT_MS, 5_000);
        assert_eq!(effective_script_timeout_ms(5_000, None, None), 5_000);
        assert_eq!(effective_script_timeout_ms(5_000, Some(6_000), None), 6_000);
        assert_eq!(effective_script_timeout_ms(5_000, Some(3_000), Some(7_000)), 7_000);
        // A negative setting/generator timeout never wraps into a huge Rust
        // duration; JS's setTimeout path observes the resulting zero.
        assert_eq!(effective_script_timeout_ms(-1_000, Some(-100), None), 0);
    }

    #[test]
    fn npm_run_reads_package_scripts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest","lint":"eslint .","build":"tsc"}}"#,
        )
        .unwrap();
        let spec = Spec {
            names: vec!["npm".into()],
            args: vec![ArgSpec {
                builtin: Some(Builtin::NpmScripts),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let cwd = dir.path().display().to_string();
        let suggestions = generate(&spec, &["npm".into(), "run".into(), "t".into()], "t", &cwd, false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["test"]);
        let again = generate(&spec, &["npm".into(), "run".into(), "t".into()], "t", &cwd, false);
        assert_eq!(again.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["test"]);
    }

    #[test]
    fn mtime_lru_keeps_fresh_and_evicts_stale() {
        let mut cache = MtimeLru::default();
        let t1 = SystemTime::UNIX_EPOCH;
        cache.insert("a".into(), Some(t1), "one".to_string());
        assert_eq!(cache.get("a", Some(t1)).as_deref(), Some("one"));
        assert_eq!(cache.get("a", Some(t1 + Duration::from_secs(1))), None);
        cache.insert("a".into(), Some(t1 + Duration::from_secs(1)), "two".to_string());
        assert_eq!(
            cache.get("a", Some(t1 + Duration::from_secs(1))).as_deref(),
            Some("two")
        );
        for i in 0..MAX_CACHED {
            cache.insert(format!("k{i}"), Some(t1), i.to_string());
        }
        assert_eq!(cache.get("a", Some(t1 + Duration::from_secs(1))), None);
        let last = (MAX_CACHED - 1).to_string();
        assert_eq!(
            cache.get(&format!("k{}", MAX_CACHED - 1), Some(t1)).as_deref(),
            Some(last.as_str())
        );
    }

    #[test]
    fn git_refs_mtime_changes_when_a_branch_file_is_added() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let before = git_refs_mtime(&git);
        std::thread::sleep(Duration::from_millis(20));
        fs::write(git.join("refs/heads/feature"), "abc\n").unwrap();
        let after = git_refs_mtime(&git);
        assert_ne!(before, after, "new branch must bust the git-ref cache");
    }

    #[test]
    fn find_git_dir_resolves_worktree_gitdir_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.git");
        fs::create_dir_all(real.join("refs/heads")).unwrap();
        fs::write(real.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        fs::write(work.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        let found = find_git_dir(&work.display().to_string()).expect("worktree git dir");
        assert_eq!(found, real);
        assert!(git_refs_mtime(&found).is_some());
    }

    #[test]
    fn git_ref_output_is_prefix_filtered() {
        let spec = Spec {
            names: vec!["checkout".into()],
            args: vec![ArgSpec {
                script: vec!["printf".into(), "feature-x\nmain\norigin/foo\n".into()],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(
            &spec,
            &["git".into(), "checkout".into(), "fea".into()],
            "fea",
            "/",
            false,
        );
        assert_eq!(
            suggestions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["feature-x"]
        );
    }

    #[test]
    fn git_alias_output_is_normalized_like_the_fig_generator() {
        let alias = parse_git_alias("alias.co checkout -b main").expect("alias");
        assert_eq!(alias.name, "co");
        assert_eq!(alias.description, "Alias for 'checkout -b main'");
        assert_eq!(alias.kind, "arg");
        assert!(parse_git_alias("not-an-alias").is_none());
    }

    #[test]
    fn git_alias_builtin_reads_aliases_from_the_worktree_config() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let initialized = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&cwd)
            .status()
            .expect("git");
        assert!(initialized.success(), "git init failed");
        let configured = std::process::Command::new("git")
            .args(["config", "alias.co", "checkout -b main"])
            .current_dir(&cwd)
            .status()
            .expect("git");
        assert!(configured.success(), "git config failed");

        let spec = Spec {
            names: vec!["git".into()],
            args: vec![ArgSpec {
                builtin: Some(Builtin::GitAliases),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["git".into()], "co", &cwd, false);
        assert_eq!(
            suggestions
                .iter()
                .map(|suggestion| suggestion.name.as_str())
                .collect::<Vec<_>>(),
            vec!["co"]
        );
        assert_eq!(suggestions[0].description, "Alias for 'checkout -b main'");
    }

    #[test]
    fn folders_template_lists_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("readme.md"), "x").unwrap();
        let spec = Spec {
            names: vec!["cd".into()],
            args: vec![ArgSpec {
                templates: vec![Template::Folders],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(
            &spec,
            &["cd".into(), "s".into()],
            "s",
            &dir.path().display().to_string(),
            false,
        );
        assert!(suggestions.iter().any(|s| s.name == "src/"), "{suggestions:?}");
        assert!(suggestions.iter().all(|s| s.kind != "file"));
    }

    #[test]
    fn option_args_are_completed_after_the_flag() {
        let spec = Spec {
            names: vec!["ls".into()],
            options: vec![OptionSpec {
                names: vec!["--color".into()],
                args: vec![ArgSpec {
                    suggestions: vec![
                        SuggestionSeed {
                            names: vec!["always".into()],
                            description: String::new(),
                            ..SuggestionSeed::default()
                        },
                        SuggestionSeed {
                            names: vec!["never".into()],
                            description: String::new(),
                            ..SuggestionSeed::default()
                        },
                    ],
                    ..ArgSpec::default()
                }],
                ..OptionSpec::default()
            }],
            args: vec![ArgSpec {
                templates: vec![Template::Filepaths],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["ls".into(), "--color".into(), "a".into()], "a", "/", false);
        let names: Vec<_> = suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["always"]);
    }

    #[test]
    fn only_the_active_positional_arg_runs_its_generator() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![
                ArgSpec {
                    suggestions: vec![SuggestionSeed {
                        names: vec!["alpha".into()],
                        ..SuggestionSeed::default()
                    }],
                    ..ArgSpec::default()
                },
                ArgSpec {
                    suggestions: vec![SuggestionSeed {
                        names: vec!["beta".into()],
                        ..SuggestionSeed::default()
                    }],
                    ..ArgSpec::default()
                },
            ],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "alpha".into(), "b".into()], "b", "/", false);
        assert_eq!(
            suggestions.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["beta"]
        );
    }

    #[test]
    fn duplicate_static_and_builtin_rows_are_removed() {
        let spec = Spec {
            names: vec!["demo".into()],
            args: vec![ArgSpec {
                suggestions: vec![
                    SuggestionSeed {
                        names: vec!["same".into()],
                        ..SuggestionSeed::default()
                    },
                    SuggestionSeed {
                        names: vec!["same".into()],
                        ..SuggestionSeed::default()
                    },
                ],
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let suggestions = generate(&spec, &["demo".into(), "s".into()], "s", "/", false);
        assert_eq!(suggestions.iter().filter(|item| item.name == "same").count(), 1);
    }

    #[test]
    fn git_native_generators_use_their_narrow_data_sources() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().display().to_string();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&cwd)
                .status()
                .expect("git");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "ec@example.invalid"]);
        run(&["config", "user.name", "Easy Complete"]);
        fs::write(dir.path().join("tracked.txt"), "initial").unwrap();
        run(&["add", "tracked.txt"]);
        run(&["commit", "-qm", "initial commit"]);
        run(&["branch", "feature"]);
        run(&["tag", "v1"]);
        run(&["remote", "add", "origin", "https://example.invalid/repo.git"]);
        fs::write(dir.path().join("untracked.txt"), "pending").unwrap();

        let arg = |builtin| ArgSpec {
            builtin: Some(builtin),
            ..ArgSpec::default()
        };
        let branches = generate_for_arg(&arg(Builtin::GitBranches), &["git".into()], "fea", &cwd, false);
        assert!(branches.iter().any(|item| item.name == "feature"), "{branches:?}");
        let tags = generate_for_arg(&arg(Builtin::GitTags), &["git".into()], "v", &cwd, false);
        assert!(tags.iter().any(|item| item.name == "v1"), "{tags:?}");
        let commits = generate_for_arg(&arg(Builtin::GitCommits), &["git".into()], "", &cwd, false);
        assert!(
            commits.iter().any(|item| item.description.contains("initial commit")),
            "{commits:?}"
        );
        let remotes = generate_for_arg(&arg(Builtin::GitRemotes), &["git".into()], "o", &cwd, false);
        assert_eq!(
            remotes.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["origin"]
        );
        let changed = generate_for_arg(&arg(Builtin::GitChangedFiles), &["git".into()], "u", &cwd, false);
        assert!(changed.iter().any(|item| item.name == "untracked.txt"), "{changed:?}");
    }

    #[test]
    fn docker_exec_post_process_parses_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        fs::write(
            hooks.join("docker_postProcess_6.js"),
            "export default t=>t.split(`\n`).map(n=>{try{let i=JSON.parse(n);return{name:i.Names,displayName:`${i.Names} (${i.Image})`,icon:\"fig://icon?type=docker\"}}catch(i){console.error(i)}});\n",
        )
        .unwrap();
        let host = crate::js_host::JsHost::new(hooks);
        let stdout = "{\"Names\":\"web\",\"Image\":\"nginx\"}\n{\"Names\":\"db\",\"Image\":\"postgres\"}\n";
        let rows = host
            .post_process("docker#postProcess#6", stdout, &["docker".into(), "exec".into()])
            .expect("rows");
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["web", "db"]);
        assert_eq!(rows[0].display_name.as_deref(), Some("web (nginx)"));
    }

    #[test]
    fn js_post_process_maps_stdout_and_skips_empty_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        fs::write(
            hooks.join("demo_postProcess_0.js"),
            "export default function(out) { return out.split('\\n').filter(Boolean).map((line) => ({ name: 'x-' + line })); }\n",
        )
        .unwrap();
        let host = crate::js_host::JsHost::new(hooks);
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha\nbeta\n".into()],
            js_post_process: Some("demo#postProcess#0".into()),
            ..ArgSpec::default()
        };
        let rows = host.enter("/", || generate_for_arg(&arg, &["demo".into()], "", "/", false));
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&"x-alpha"), "{names:?}");
        assert!(names.contains(&"x-beta"), "{names:?}");

        let empty = host.enter("", || generate_for_arg(&arg, &["demo".into()], "", "", false));
        assert!(empty.is_empty(), "{empty:?}");
    }

    #[test]
    fn split_on_wins_over_post_process_like_the_webview() {
        // Fig's `getScriptSuggestions` is `if (splitOn) … else if (postProcess)`.
        // A spec that declares both never sees its hook run.
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        fs::write(
            hooks.join("demo_postProcess_0.js"),
            "export default function() { return [{ name: 'from-hook' }]; }\n",
        )
        .unwrap();
        let host = crate::js_host::JsHost::new(hooks);
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha,beta".into()],
            split_on: Some(",".into()),
            js_post_process: Some("demo#postProcess#0".into()),
            ..ArgSpec::default()
        };
        let rows = host.enter("/", || generate_for_arg(&arg, &["demo".into()], "", "/", false));
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn js_post_process_errors_become_empty() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        fs::write(
            hooks.join("demo_postProcess_0.js"),
            "export default function() { throw new Error('boom'); }\n",
        )
        .unwrap();
        let host = crate::js_host::JsHost::new(hooks);
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha\n".into()],
            js_post_process: Some("demo#postProcess#0".into()),
            ..ArgSpec::default()
        };
        let rows = host.enter("/", || generate_for_arg(&arg, &["demo".into()], "", "/", false));
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn js_custom_and_cache_avoid_repeat_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let count = dir.path().join("count");
        fs::write(
            hooks.join("demo_custom_0.js"),
            "export default async function(tokens, exec, ctx) {\n  await exec({ command: 'sh', args: ['-c', 'echo x >> \"' + ctx.currentWorkingDirectory + '/count\"'] });\n  return [{ name: 'from-custom' }];\n}\n",
        )
        .unwrap();
        let host = crate::js_host::JsHost::new(hooks);
        let cwd = dir.path().display().to_string();
        let arg = ArgSpec {
            js_custom: Some("demo#custom#0".into()),
            cache_key: Some("custom".into()),
            cache_ttl_ms: Some(60_000),
            ..ArgSpec::default()
        };
        let first = host.enter(&cwd, || generate_for_arg(&arg, &["demo".into()], "", &cwd, false));
        let second = host.enter(&cwd, || generate_for_arg(&arg, &["demo".into()], "", &cwd, false));
        assert_eq!(
            first.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["from-custom"]
        );
        assert_eq!(
            second.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["from-custom"]
        );
        let written = fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(written.matches('x').count(), 1, "{written}");
    }

    #[test]
    fn cached_script_rows_are_refiltered_when_the_query_changes() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        fs::write(
            hooks.join("demo_postProcess_0.js"),
            "export default function(out) { return out.split('\\n').filter(Boolean).map((line) => ({ name: line })); }\n",
        )
        .unwrap();
        let host = crate::js_host::JsHost::new(hooks);
        let count = dir.path().join("count");
        let script = format!("printf 'web\\napi\\nwest\\n'; echo x >> '{}'", count.display());
        let arg = ArgSpec {
            script: vec!["sh".into(), "-c".into(), script],
            js_post_process: Some("demo#postProcess#0".into()),
            cache_key: Some("rows".into()),
            cache_ttl_ms: Some(60_000),
            ..ArgSpec::default()
        };
        let cwd = dir.path().display().to_string();
        let first = host.enter(&cwd, || {
            generate_for_arg(&arg, &["demo".into(), "w".into()], "w", &cwd, false)
        });
        let second = host.enter(&cwd, || {
            generate_for_arg(&arg, &["demo".into(), "we".into()], "we", &cwd, false)
        });
        assert_eq!(
            first.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["web", "west"]
        );
        assert_eq!(
            second.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["web", "west"]
        );
        let written = fs::read_to_string(&count).unwrap_or_default();
        assert_eq!(written.matches('x').count(), 1, "{written}");
    }

    #[test]
    fn native_script_still_runs_with_an_empty_cwd_when_there_is_no_js_hook() {
        let dir = tempfile::tempdir().unwrap();
        let host = crate::js_host::JsHost::new(dir.path().join("hooks"));
        let arg = ArgSpec {
            script: vec!["printf".into(), "alpha\n".into()],
            ..ArgSpec::default()
        };
        let rows = host.enter("", || generate_for_arg(&arg, &["demo".into()], "", "", false));
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha"]
        );
    }

    #[test]
    fn git_branch_generator_omits_remote_head_symbolic_ref() {
        let rows = "main\t\norigin\trefs/remotes/origin/main\norigin/main\t\n";
        let suggestions = branch_lines_to_suggestions(rows, "", false);
        assert_eq!(
            suggestions.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "origin/main"]
        );
    }
}
