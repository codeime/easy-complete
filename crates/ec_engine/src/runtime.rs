//! Completion engine: load spec IR and run lookup.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ir::Registry;
use crate::js_host;
use crate::lookup;
use crate::rank::{self, Frecency};

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn history_loading_enabled(disabled: bool) -> bool {
    !disabled
}

fn should_merge_history(include_history: bool, disabled: bool) -> bool {
    include_history && history_loading_enabled(disabled)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRequest {
    pub buffer: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub cursor: Option<u32>,
    #[serde(default)]
    pub fuzzy: bool,
    #[serde(default)]
    pub history_only: bool,
    #[serde(default = "default_true")]
    pub include_history: bool,
    #[serde(default = "default_false")]
    pub suggest_first_token: bool,
    /// Shell executable/path for selecting shell-specific history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_shell: Option<String>,
    /// Current process name/path, used when the integration does not expose a
    /// dedicated shell field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_process: Option<String>,
    /// Shell environment reported by the terminal integration. Fig `custom`
    /// generators read it through `context.environmentVariables`.
    ///
    /// Shared across the overlay request and the JS host so a keystroke does
    /// not clone every `KEY=value` pair.
    #[serde(default, skip_serializing_if = "empty_env")]
    pub environment_variables: Arc<Vec<(String, String)>>,
}

fn empty_env(value: &Arc<Vec<(String, String)>>) -> bool {
    value.is_empty()
}

impl Default for CompleteRequest {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            cwd: String::new(),
            cursor: None,
            fuzzy: false,
            history_only: false,
            include_history: true,
            suggest_first_token: false,
            current_shell: None,
            current_process: None,
            environment_variables: Arc::new(Vec::new()),
        }
    }
}

/// Settings consumed on one `complete`. Snapshotted once so lookup/runtime
/// do not call `get_bool_or` for the same keys on every auto-execute path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AutocompleteFlags {
    pub hide_auto_execute: bool,
    pub only_show_on_tab: bool,
    pub immediately_run_dangerous: bool,
    pub prefer_verbose: bool,
    pub always_suggest_current_token: bool,
    pub immediately_execute_after_space: bool,
    pub history_disable_loading: bool,
    pub history_all_shells: bool,
    pub history_custom_command: Option<String>,
    pub alphabetical: bool,
    pub disable_for_commands: Vec<String>,
}

impl AutocompleteFlags {
    pub(crate) fn snapshot() -> Self {
        let settings = fig_settings::settings::Settings::new();
        let custom = settings.get_string_or("beta.history.customCommand", String::new());
        Self {
            hide_auto_execute: settings.get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
            only_show_on_tab: settings.get_bool_or("autocomplete.onlyShowOnTab", false),
            immediately_run_dangerous: settings.get_bool_or("autocomplete.immediatelyRunDangerousCommands", false),
            prefer_verbose: settings.get_bool_or("autocomplete.preferVerboseSuggestions", false),
            always_suggest_current_token: settings.get_bool_or("autocomplete.alwaysSuggestCurrentToken", false),
            immediately_execute_after_space: settings.get_bool_or("autocomplete.immediatelyExecuteAfterSpace", false),
            history_disable_loading: settings.get_bool_or("autocomplete.history.disableLoading", false),
            history_all_shells: settings.get_bool_or("beta.history.allShells", false),
            history_custom_command: (!custom.is_empty()).then_some(custom),
            alphabetical: settings.get_string_or("autocomplete.sortMethod", "default".into()) == "alphabetical",
            disable_for_commands: settings
                .get::<Vec<String>>("autocomplete.disableForCommands")
                .ok()
                .flatten()
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Suggestion {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub args_hint: String,
    /// Explicit shell text.  This is intentionally separate from `name`: Fig
    /// suggestions often display a friendly label but insert a command, an
    /// option alias, or a history line instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The first spelling from the source suggestion's `name` array.  The
    /// WebView uses this primary name when deciding whether an exact alias
    /// may receive an auto-execute row, even when a different alias was
    /// selected for display/insertion.
    #[serde(skip)]
    pub primary_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator_to_add: Option<String>,
    #[serde(default)]
    pub should_add_space: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_type: Option<String>,
    /// The query term after applying a static string `getQueryTerm` rule.
    /// This is per suggestion because a single result can contain rows from
    /// different generators, each with a different delimiter.  The raw
    /// [`CompleteResult::search_term`] remains available for rows without a
    /// query-term override and for shell deletion bookkeeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_term: Option<String>,
    #[serde(default)]
    pub is_dangerous: bool,
    /// Internal parser fact used to decide whether an exact row may become an
    /// auto-execute action. `args_hint` cannot carry this reliably because Fig
    /// permits mandatory arguments without a display name.
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_arg: bool,
}

impl Suggestion {
    pub fn new(name: impl Into<String>, description: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: kind.into(),
            args_hint: String::new(),
            insert_value: None,
            display_name: None,
            primary_name: None,
            separator_to_add: None,
            should_add_space: false,
            hidden: false,
            // Fig's priority normalizer treats an omitted/zero priority as 50.
            priority: 50,
            icon: None,
            original_type: None,
            query_term: None,
            is_dangerous: false,
            requires_arg: false,
        }
    }

    pub fn with_args_hint(mut self, hint: impl Into<String>) -> Self {
        self.args_hint = hint.into();
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_meta(
        mut self,
        insert_value: Option<String>,
        display_name: Option<String>,
        separator_to_add: Option<String>,
        should_add_space: bool,
        hidden: bool,
        priority: Option<i64>,
        icon: Option<String>,
    ) -> Self {
        self.insert_value = insert_value;
        self.display_name = display_name;
        self.separator_to_add = separator_to_add;
        self.should_add_space = should_add_space;
        self.hidden = hidden;
        self.priority = priority.map_or(50, normalize_priority);
        self.icon = icon;
        self
    }

    pub fn with_dangerous(mut self, dangerous: bool) -> Self {
        self.is_dangerous = dangerous;
        self
    }

    pub fn with_insert_value(mut self, value: impl Into<String>) -> Self {
        self.insert_value = Some(value.into());
        self
    }

    pub fn with_primary_name(mut self, value: Option<String>) -> Self {
        self.primary_name = value;
        self
    }

    pub fn with_original_type(mut self, original_type: Option<String>) -> Self {
        self.original_type = original_type;
        self
    }

    pub fn with_query_term(mut self, query_term: Option<String>) -> Self {
        self.query_term = query_term;
        self
    }

    pub fn with_priority(mut self, priority: i64) -> Self {
        self.priority = normalize_priority(priority);
        self
    }
}

fn normalize_priority(priority: i64) -> i64 {
    if priority == 0 { 50 } else { priority.clamp(0, 100) }
}

/// Apply the serializable subset of Fig's `getQueryTerm` contract.
///
/// The WebView helper uses `searchTerm.slice(lastIndexOf(separator) + 1)`
/// rather than the separator's full length.  Keep that exact behavior for
/// compatibility; bundled separators are normally one character (`/`, `:`,
/// `,`, or `=`).  A missing separator leaves the whole search term intact.
pub fn query_term_for(search_term: &str, separator: Option<&str>) -> String {
    let Some(separator) = separator else {
        return search_term.to_string();
    };
    if separator.is_empty() {
        return String::new();
    }
    let Some(index) = search_term.rfind(separator) else {
        return search_term.to_string();
    };
    let Some(first) = search_term[index..].chars().next() else {
        return String::new();
    };
    search_term[index + first.len_utf8()..].to_string()
}

/// Compute the matching term for one static suggestion. Explicit string
/// getQueryTerm has priority; shortcut rows use the legacy `?` prefix rule
/// only when no explicit query-term override is present.
pub(crate) fn suggestion_query_term(
    kind: &str,
    explicit_separator: Option<&str>,
    query: &str,
    search_term: &str,
) -> (String, Option<String>) {
    if let Some(separator) = explicit_separator {
        let term = query_term_for(search_term, Some(separator));
        return (term.clone(), Some(term));
    }
    if kind == "shortcut" && search_term.starts_with('?') {
        let term = search_term[1..].to_string();
        return (term.clone(), Some(term));
    }
    (query.to_string(), None)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CurrentArg {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompleteResult {
    pub suggestions: Vec<Suggestion>,
    /// Effective fuzzy/prefix mode after applying the current spec/argument
    /// filterStrategy. This is distinct from CompleteRequest::fuzzy, which is
    /// only the user's setting.
    #[serde(default)]
    pub fuzzy: bool,
    #[serde(default)]
    pub search_term: String,
    /// Normalized token used for matching/ranking. `search_term` remains the
    /// raw shell text so the overlay can delete exactly what is under the
    /// caret (including quotes and escaped spaces).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub match_term: String,
    /// The argument currently being completed.  This is intentionally small:
    /// the overlay uses it as a fallback description when no suggestion is
    /// selected (for example a required special argument with no results).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_arg: Option<CurrentArg>,
}

/// Return the root-command key used by native ranking for this edit buffer.
/// Acceptance recording must use the same normalized token as completion
/// ranking, otherwise quoted commands and a cursor before the buffer end
/// would write into a different recency bucket.
pub fn ranking_root_command(buffer: &str, cursor: Option<u32>) -> String {
    lookup::tokenize(lookup::completion_buffer(buffer, cursor))
        .0
        .into_iter()
        .next()
        .unwrap_or_default()
}

pub struct Engine {
    registry: Registry,
    js_host: crate::js_host::JsHost,
    frecency: Frecency,
    acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    frecency_loaded: bool,
    history_source: Option<rank::HistorySourceConfig>,
}

impl Engine {
    pub fn new(specs_dir: PathBuf) -> anyhow::Result<Self> {
        Self::new_with_acceptance(specs_dir, Arc::new(Mutex::new(rank::AcceptanceIndex::load())))
    }

    pub(crate) fn new_with_acceptance(
        specs_dir: PathBuf,
        acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    ) -> anyhow::Result<Self> {
        let registry = Self::load_registry(&specs_dir)?;
        Ok(Self::from_registry(&specs_dir, registry, acceptance))
    }

    /// Index the specs directory without parsing any spec: `Registry` resolves
    /// files lazily, so this is only `index.json` plus a directory walk.
    pub(crate) fn load_registry(specs_dir: &Path) -> anyhow::Result<Registry> {
        if specs_dir.is_dir() {
            Registry::load(specs_dir)
        } else {
            Ok(Registry::new())
        }
    }

    /// Build around an existing index. The supervisor uses this to recover from
    /// a timed-out attempt without touching the specs directory again.
    pub(crate) fn from_registry(
        specs_dir: &Path,
        registry: Registry,
        acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    ) -> Self {
        Self {
            registry,
            js_host: crate::js_host::JsHost::from_specs_dir(specs_dir),
            frecency: Frecency::default(),
            acceptance,
            frecency_loaded: false,
            history_source: None,
        }
    }

    pub fn new_with_frecency(specs_dir: PathBuf, frecency: Frecency) -> anyhow::Result<Self> {
        Self::new_with_frecency_and_acceptance(
            specs_dir,
            frecency,
            // This constructor is used by tests and headless embeddings that
            // provide their own ranking input. Keep it deterministic and do
            // not read or write the user's acceptance database.
            Arc::new(Mutex::new(rank::AcceptanceIndex::default())),
        )
    }

    pub(crate) fn new_with_frecency_and_acceptance(
        specs_dir: PathBuf,
        frecency: Frecency,
        acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    ) -> anyhow::Result<Self> {
        let js_host = crate::js_host::JsHost::from_specs_dir(&specs_dir);
        let registry = if specs_dir.is_dir() {
            Registry::load(&specs_dir)?
        } else {
            Registry::new()
        };
        Ok(Self {
            registry,
            js_host,
            frecency,
            acceptance,
            frecency_loaded: true,
            // `new_with_frecency` is the test/embedding constructor that
            // intentionally supplies its own ranking data. Treat it as the
            // default source until a request asks for a different shell or
            // history setting.
            history_source: Some(rank::HistorySourceConfig {
                custom_command: None,
                all_shells: false,
                current_shell: rank::HistoryShell::Unknown,
            }),
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Record a successful completion acceptance. This updates the engine's
    /// in-memory ranking immediately and best-effort persists it to the shared
    /// SQLite-backed state store.
    pub fn record_acceptance(&mut self, root_command: &str, accepted_name: &str) {
        let timestamp = rank::AcceptanceIndex::now_millis();
        self.record_acceptance_at(root_command, accepted_name, timestamp);
    }

    pub(crate) fn record_acceptance_at(&mut self, root_command: &str, accepted_name: &str, timestamp: u64) {
        // Persist from a snapshot taken outside the lock: ranking clones this
        // index on every completion, and a slow SQLite write while holding the
        // mutex would stall those attempts.
        let snapshot = {
            let mut acceptance = self.acceptance.lock().unwrap_or_else(|err| err.into_inner());
            acceptance
                .record_at(root_command, accepted_name, timestamp)
                .then(|| acceptance.clone())
        };
        if let Some(snapshot) = snapshot {
            snapshot.persist();
        }
    }

    fn ensure_frecency(&mut self, request: &CompleteRequest, flags: &AutocompleteFlags) {
        let source = rank::history_source_config(
            flags.history_custom_command.clone(),
            flags.history_all_shells,
            request.current_shell.as_deref(),
            request.current_process.as_deref(),
        );
        if self.frecency_loaded && self.history_source.as_ref() == Some(&source) {
            return;
        }
        self.frecency = Frecency::from_commands(rank::load_commands_for(&source));
        self.frecency_loaded = true;
        self.history_source = Some(source);
    }

    pub fn complete(&mut self, mut request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        let buffer = lookup::completion_buffer(&request.buffer, request.cursor);
        let (tokens, ends_with_space) = lookup::tokenize(buffer);

        let flags = AutocompleteFlags::snapshot();
        let history_disabled = flags.history_disable_loading;
        if history_loading_enabled(history_disabled) {
            self.ensure_frecency(&request, &flags);
        }
        if request.history_only {
            let history_search_term = if ends_with_space {
                String::new()
            } else {
                lookup::current_token_raw(buffer)
            };
            let history_match_term = if ends_with_space {
                String::new()
            } else {
                tokens.last().cloned().unwrap_or_default()
            };
            let effective_fuzzy = lookup::effective_fuzzy_for_tokens(
                &mut self.registry,
                request.fuzzy,
                &tokens,
                ends_with_space,
                &history_match_term,
                &history_search_term,
            );
            if history_disabled {
                return Ok(CompleteResult {
                    suggestions: Vec::new(),
                    fuzzy: effective_fuzzy,
                    search_term: history_search_term,
                    match_term: String::new(),
                    current_arg: None,
                });
            }
            let prefix = rank::history_prefix(&tokens, &history_match_term);
            let query = history_match_term.as_str();
            let suggestions = prefix.map_or_else(
                || self.frecency.history_suggestions(query, effective_fuzzy, true),
                |prefix| {
                    self.frecency
                        .history_suffix_suggestions(&prefix, query, effective_fuzzy)
                },
            );
            return Ok(CompleteResult {
                suggestions,
                fuzzy: effective_fuzzy,
                search_term: history_search_term,
                match_term: history_match_term,
                current_arg: None,
            });
        }
        let mut result = {
            let shell = js_host::ShellContext {
                current_process: request.current_process.clone().unwrap_or_default(),
                environment_variables: std::mem::take(&mut request.environment_variables),
            };
            let host = &self.js_host;
            let registry = &mut self.registry;
            host.enter_with_context(&request.cwd, &shell, || lookup::complete(registry, &request, &flags))
        };
        if should_merge_history(request.include_history, history_disabled) {
            let effective_fuzzy = result.fuzzy;
            rank::merge_history(&mut result, &tokens, &self.frecency, effective_fuzzy);
        }
        let alphabetical = flags.alphabetical;
        let root_command = ranking_root_command(&request.buffer, request.cursor);
        {
            let acceptance = self.acceptance.lock().unwrap_or_else(|err| err.into_inner());
            if history_disabled {
                // A setting can change while the engine stays alive. Do not let
                // frecency loaded by an earlier request influence ranking after
                // history loading has been disabled.
                rank::apply_with_acceptance(
                    &mut result,
                    &tokens,
                    &Frecency::default(),
                    &acceptance,
                    &root_command,
                    alphabetical,
                );
            } else {
                rank::apply_with_acceptance(
                    &mut result,
                    &tokens,
                    &self.frecency,
                    &acceptance,
                    &root_command,
                    alphabetical,
                );
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_spec(dir: &std::path::Path, name: &str, body: &str) {
        fs::write(dir.join(format!("{name}.json")), body).unwrap();
    }

    #[test]
    fn autocomplete_flags_default_matches_documented_settings_defaults() {
        let flags = AutocompleteFlags::default();
        assert!(!flags.hide_auto_execute);
        assert!(!flags.only_show_on_tab);
        assert!(!flags.immediately_run_dangerous);
        assert!(!flags.prefer_verbose);
        assert!(!flags.always_suggest_current_token);
        assert!(!flags.immediately_execute_after_space);
        assert!(!flags.history_disable_loading);
        assert!(!flags.history_all_shells);
        assert!(flags.history_custom_command.is_none());
        assert!(!flags.alphabetical);
        assert!(flags.disable_for_commands.is_empty());
    }

    #[test]
    fn omitted_priority_matches_webview_default() {
        assert_eq!(Suggestion::new("status", "", "subcommand").priority, 50);
        assert_eq!(
            Suggestion::new("status", "", "subcommand")
                .with_meta(None, None, None, false, false, None, None)
                .priority,
            50
        );
        assert_eq!(Suggestion::new("zero", "", "arg").with_priority(0).priority, 50);
        assert_eq!(Suggestion::new("high", "", "arg").with_priority(900).priority, 100);
        assert_eq!(Suggestion::new("low", "", "arg").with_priority(-2).priority, 0);
    }

    #[test]
    fn disable_history_loading_prevents_loading_and_merging() {
        assert!(history_loading_enabled(false));
        assert!(!history_loading_enabled(true));
        assert!(should_merge_history(true, false));
        assert!(!should_merge_history(true, true));
        assert!(!should_merge_history(false, false));
    }

    #[test]
    fn string_query_term_matches_webview_separator_behavior() {
        assert_eq!(query_term_for("~/foo", Some("/")), "foo");
        assert_eq!(query_term_for("a/b/c", Some("/")), "c");
        assert_eq!(query_term_for("foo", Some("/")), "foo");
        assert_eq!(query_term_for("foo", None), "foo");
        assert_eq!(query_term_for("foo", Some("")), "");
    }

    #[test]
    fn ranking_root_command_matches_the_completion_token() {
        assert_eq!(ranking_root_command("git checkout", None), "git");
        assert_eq!(ranking_root_command("'git' checkout", None), "git");
        assert_eq!(ranking_root_command(r"my\ command arg", None), "my command");
        assert_eq!(ranking_root_command("git checkout", Some(2)), "gi");
        assert_eq!(ranking_root_command("", None), "");
        assert_eq!(ranking_root_command("echo x && git checkout", None), "git");
        assert_eq!(ranking_root_command("FOO=1 git checkout", None), "git");
        assert_eq!(ranking_root_command("echo x && git checkout", Some(6)), "echo");
    }

    #[test]
    fn generate_spec_merges_dynamic_subcommands() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("hooks")).unwrap();
        fs::write(
            dir.path().join("hooks/php_generateSpec_0.js"),
            "export default async function() { return { name: 'php', subcommands: [{ name: 'artisan', description: 'Laravel' }] }; }\n",
        )
        .unwrap();
        write_spec(
            dir.path(),
            "php",
            r#"{"names":["php"],"jsGenerateSpec":"php#generateSpec#0"}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "php ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|row| row.name == "artisan"),
            "{:?}",
            result.suggestions
        );
    }

    fn engine_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn completes_git_subcommands_from_ir() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "description": "the stupid content tracker",
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches or restore working tree files"},
                {"names": ["commit"], "description": "Record changes to the repository"},
                {"names": ["cherry-pick"], "description": "Apply the changes introduced by some existing commits"},
                {"names": ["clone"], "description": "Clone a repository into a new directory"},
                {"names": ["status"], "description": "Show the working tree status"}
              ],
              "options": [{"names": ["--help"], "description": "Show help"}]
            }"#,
        );

        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git ch".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");

        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "got {names:?}");
        assert!(names.contains(&"cherry-pick"), "got {names:?}");
        assert!(!names.contains(&"status"), "got {names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn history_merge_uses_active_argument_filter_strategy() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "argtool",
            r#"{
              "names":["argtool"],
              "filterStrategy":"fuzzy",
              "args":[{"name":"value"}]
            }"#,
        );
        let frecency = Frecency::from_commands([("argtool target".into(), 100)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");

        let prefix = engine
            .complete(CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: false,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("prefix completion");
        assert!(!prefix.fuzzy);
        assert!(!prefix.suggestions.iter().any(|item| item.kind == "history"));

        let history_only = engine
            .complete(CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: false,
                history_only: true,
                ..CompleteRequest::default()
            })
            .expect("history-only completion");
        assert!(!history_only.fuzzy);
        assert!(!history_only.suggestions.iter().any(|item| item.name == "target"));

        let fuzzy = engine
            .complete(CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("fuzzy completion");
        assert!(fuzzy.fuzzy);
        assert!(fuzzy.suggestions.iter().any(|item| item.kind == "history"));
    }

    #[test]
    fn completes_options_for_ls() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "ls",
            r#"{
              "names": ["ls"],
              "description": "List directory contents",
              "options": [
                {"names": ["-a"], "description": "Include directory entries whose names begin with a dot"},
                {"names": ["-l"], "description": "List in long format"},
                {"names": ["--color"], "description": "Colorize output"}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "ls -".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with('-')), "got {names:?}");
    }

    #[test]
    fn preserves_insert_metadata_and_auto_executes_exact_subcommands() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["status"], "description": "Show status", "priority": 77,
                 "icon": "📋"},
                {"names": ["commit"], "insertValue": "git commit", "args": [{"name": "message"}]}
              ],
              "options": [{"names": ["--message"], "requiresEquals": true,
                            "args": [{"name": "message"}]}]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");

        let exact = engine
            .complete(CompleteRequest {
                buffer: "git status".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        let status = exact
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "subcommand")
            .expect("status suggestion");
        assert_eq!(status.insert_value, None);
        assert_eq!(status.priority, 77);
        assert_eq!(status.icon.as_deref(), Some("📋"));

        let partial = engine
            .complete(CompleteRequest {
                buffer: "git co".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        let commit = partial
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "commit")
            .expect("commit suggestion");
        assert_eq!(commit.insert_value.as_deref(), Some("git commit"));

        let options = engine
            .complete(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        let message = options
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "--message")
            .expect("--message option");
        assert_eq!(message.separator_to_add.as_deref(), Some("="));
        assert!(message.should_add_space);
    }

    #[test]
    fn completes_bundled_mkdir_ir() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "mkdir",
            r#"{
              "names": ["mkdir"],
              "description": "Make directories",
              "args": [{"name": "directory name", "templates": ["folders"]}],
              "options": [
                {"names": ["-p", "--parents"], "description": "No error if existing"},
                {"names": ["--help"], "description": "Display this help and exit"}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "mkdir -".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.iter().any(|n| *n == "-p" || *n == "--parents"), "got {names:?}");
    }

    #[test]
    fn completes_cd_folders() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("readme.md"), "x").unwrap();
        write_spec(
            dir.path(),
            "cd",
            r#"{"names":["cd"],"args":[{"templates":["folders"]}]}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "cd s".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|s| s.name == "src/"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn completes_npm_run_scripts() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"vitest","lint":"eslint"}}"#,
        )
        .unwrap();
        write_spec(
            dir.path(),
            "npm",
            r#"{
              "names": ["npm"],
              "subcommands": [
                {"names": ["run", "run-script"], "args": [{"builtin": "npm-scripts"}]}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "npm run t".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["test"]);
    }

    #[test]
    fn cobra_fallback_for_unknown_command() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fakecobra");
        std::fs::write(&bin, "#!/bin/sh\nprintf 'alpha\\tfirst\\nalpaca\\n:4\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: format!("{} al", bin.display()),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"), "{names:?}");
        assert!(names.contains(&"alpaca"), "{names:?}");
        assert!(!names.contains(&"beta"), "{names:?}");
    }

    #[test]
    fn ranks_frequent_history_subcommand_first() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["cherry-pick"], "description": "Apply commits"}
              ]
            }"#,
        );
        let frecency = Frecency::from_commands([
            ("git checkout".into(), 10),
            ("git checkout".into(), 20),
            ("git checkout".into(), 30),
            ("git cherry-pick".into(), 1),
        ]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git ch".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.suggestions[0].name, "checkout");
    }

    #[test]
    fn chained_buffer_ranks_and_merges_history_for_the_current_command() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["cherry-pick"], "description": "Apply commits"}
              ]
            }"#,
        );
        let frecency = Frecency::from_commands([
            ("git checkout".into(), 10),
            ("git checkout".into(), 20),
            ("git checkout".into(), 30),
            ("git cherry-pick".into(), 1),
            ("git checkout -b feature".into(), 40),
        ]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "echo hello && git ch".into(),
                cwd: "/".into(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.suggestions[0].name, "checkout");
        assert_eq!(ranking_root_command("echo hello && git ch", None), "git");
        let history = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "history")
            .expect("history suffix for the current git command");
        assert_eq!(history.name, "checkout -b feature");
        assert!(!history.name.contains("echo"));
    }

    #[test]
    fn first_token_history_keeps_search_term_for_insert() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                cursor: None,
                suggest_first_token: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.search_term, "git");
        let history = result
            .suggestions
            .iter()
            .find(|s| s.kind == "history")
            .expect("history suggestion");
        assert!(history.name.starts_with("git checkout"));
        assert!(history.name.starts_with(&result.search_term));
    }

    #[test]
    fn history_only_uses_the_typed_command_prefix_as_a_suffix() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git commit -m feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git co".into(),
                history_only: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let history = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "history")
            .expect("history suggestion");
        assert_eq!(history.name, "commit -m feature");
        assert_eq!(history.insert_value.as_deref(), Some("commit -m feature"));
        assert!(!history.name.starts_with("git "));
    }

    #[test]
    fn history_only_on_a_chained_buffer_uses_the_current_command_prefix() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git commit -m feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "echo x && git co".into(),
                history_only: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let history = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.kind == "history")
            .expect("history suggestion");
        assert_eq!(history.name, "commit -m feature");
        assert_eq!(result.search_term, "co");
    }

    #[test]
    fn history_only_keeps_raw_search_text_for_insertion() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        let frecency = Frecency::from_commands([("echo my file".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: r"echo my\ f".into(),
                history_only: true,
                include_history: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert_eq!(result.search_term, r"my\ f");
        assert_eq!(result.match_term, "my f");
        assert!(result.suggestions.iter().any(|item| item.name == "my file"));
    }

    #[test]
    fn first_token_completion_defaults_off_but_explicit_true_still_works() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "git", r#"{"names":["git"]}"#);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");

        let default_result = engine
            .complete(CompleteRequest {
                buffer: "gi".into(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            default_result.suggestions.is_empty(),
            "{:?}",
            default_result.suggestions
        );

        let explicit_result = engine
            .complete(CompleteRequest {
                buffer: "gi".into(),
                suggest_first_token: true,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            explicit_result
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "git")
        );
    }

    #[test]
    fn dangerous_argument_is_inherited_by_static_and_generated_rows() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "danger",
            r#"{
              "names":["danger"],
              "args":[
                {"name":"target","isDangerous":true,
                 "suggestions":[{"names":["wipe"]}],
                 "script":["printf","generated\\n"]}
              ]
            }"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");

        let static_result = engine
            .complete(CompleteRequest {
                buffer: "danger wipe".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let static_row = static_result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "wipe")
            .expect("static dangerous row");
        assert!(static_row.is_dangerous);
        assert!(
            static_result
                .suggestions
                .iter()
                .all(|suggestion| suggestion.kind != "auto-execute")
        );

        let generated_result = engine
            .complete(CompleteRequest {
                buffer: "danger gen".into(),
                cwd: dir.path().display().to_string(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        let generated_row = generated_result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "generated")
            .expect("generated dangerous row");
        assert!(generated_row.is_dangerous);
    }

    #[test]
    fn nested_path_names_keep_directory_prefix() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), "fn").unwrap();
        write_spec(
            dir.path(),
            "cat",
            r#"{"names":["cat"],"args":[{"templates":["filepaths"]}]}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "cat src/m".into(),
                cwd: dir.path().display().to_string(),
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|s| s.name == "src/main.rs"),
            "{:?}",
            result.suggestions
        );
        assert_eq!(result.search_term, "src/m");
        assert!(
            result
                .suggestions
                .iter()
                .all(|s| s.name.starts_with(&result.search_term))
        );
    }

    #[test]
    fn first_token_does_not_list_subcommands() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), Frecency::default()).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                suggest_first_token: true,
                cursor: None,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(result.suggestions.iter().all(|s| s.name != "checkout"));
        // Exact first-token may also insert an `auto-execute` wrapper named
        // `git` (Enter runs the already-typed command). The command row itself
        // stays `arg` with a raw insertValue.
        let git = result
            .suggestions
            .iter()
            .find(|s| s.name == "git" && s.kind == "arg")
            .expect("first-token command row");
        assert_eq!(git.insert_value.as_deref(), Some("git"));
        assert!(!git.should_add_space);
        assert_eq!(result.search_term, "git");
    }

    #[test]
    fn history_off_does_not_merge_history_lines() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git"],"subcommands":[{"names":["checkout"]}]}"#,
        );
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40)]);
        let mut engine = Engine::new_with_frecency(dir.path().to_path_buf(), frecency).expect("engine");
        let result = engine
            .complete(CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                cursor: None,
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().all(|s| s.kind != "history"),
            "{:?}",
            result.suggestions
        );
    }
}
