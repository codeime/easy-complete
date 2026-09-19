//! Completion engine: load spec IR and run lookup.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::hook_backend::{self, NativeHooks};
use crate::hook_types::ShellContext;
use crate::ir::Registry;
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
    /// Shared across the overlay request and native hooks so a keystroke does
    /// not clone every `KEY=value` pair.
    #[serde(default, skip_serializing_if = "empty_env")]
    pub environment_variables: Arc<Vec<(String, String)>>,
    /// Raw `alias` output from the shell integration. Fig expanded argv0
    /// from this map before walking specs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
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
            alias: None,
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
    /// Every spelling from the source `name` array. History dedup matches
    /// against this full set, the same way the WebView used `makeArray(name)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alias_names: Vec<String>,
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
            alias_names: Vec::new(),
        }
    }

    pub fn with_alias_names(mut self, names: Vec<String>) -> Self {
        self.alias_names = names;
        self
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

/// String `getQueryTerm` first; function form next. A throwing function keeps
/// the whole search term, matching `getQueryTermForSuggestion`.
pub fn query_term_with_hook(search_term: &str, separator: Option<&str>, js_hook: Option<&str>) -> String {
    if let Some(hook_id) = js_hook.filter(|id| !id.is_empty())
        && let Some(term) = crate::hook_backend::dispatch_get_query_term(hook_id, search_term)
    {
        return term;
    }
    query_term_for(search_term, separator)
}

/// Compute the matching term for one static suggestion. Explicit string
/// getQueryTerm has priority; shortcut rows use the legacy `?` prefix rule
/// only when no explicit query-term override is present.
pub(crate) fn suggestion_query_term_with_hook(
    kind: &str,
    explicit_separator: Option<&str>,
    js_hook: Option<&str>,
    query: &str,
    search_term: &str,
) -> (String, Option<String>) {
    if explicit_separator.is_some() || js_hook.is_some() {
        let term = query_term_with_hook(search_term, explicit_separator, js_hook);
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
    /// Generators were delayed by `debounce` and should be requested again
    /// after [`Self::debounce_ms`]. Static rows in this result are current.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending_generators: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_ms: Option<i64>,
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
    specs_dir: PathBuf,
    registry: Registry,
    native: Arc<NativeHooks>,
    hook_cache: Arc<crate::hook_cache::HookCache>,
    frecency: Frecency,
    acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    frecency_loaded: bool,
    history_source: Option<rank::HistorySourceConfig>,
    /// Spec-aware `template: "history"` index over the frecency lines. Rebuilt
    /// only when the loaded history changes; requests share it by `Arc`.
    history: Arc<crate::history::HistoryStore>,
    /// Generator cache that has to survive the per-request attempt thread.
    generator_session: crate::generate::GeneratorSession,
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
        // A generated IR directory is published by renaming the old tree out
        // of the way and then renaming the validated tree into place. There is
        // necessarily a very small interval in which `specs_dir` does not
        // exist. Treating that interval as a valid empty registry is unsafe:
        // the worker would cache it as its pristine template and keep serving
        // no completions after the new tree appears. Existing directories are
        // still allowed to be handwritten/legacy fixtures; that compatibility
        // path is important for headless tests and local overlays.
        let metadata = std::fs::metadata(specs_dir)
            .map_err(|error| anyhow::anyhow!("specs IR directory is unavailable: {}: {error}", specs_dir.display()))?;
        if !metadata.is_dir() {
            anyhow::bail!("specs IR path is not a directory: {}", specs_dir.display());
        }

        let mut registry = Registry::load(specs_dir)?;
        overlay_local_spec_dirs(&mut registry);
        Ok(registry)
    }

    /// Build around an existing index. The supervisor uses this to recover from
    /// a timed-out attempt without touching the specs directory again.
    pub(crate) fn from_registry(
        specs_dir: &Path,
        registry: Registry,
        acceptance: Arc<Mutex<rank::AcceptanceIndex>>,
    ) -> Self {
        let native = Arc::new(NativeHooks::load(specs_dir, registry.snapshot().as_ref()));
        Self {
            specs_dir: specs_dir.to_path_buf(),
            registry,
            native,
            hook_cache: crate::hook_cache::HookCache::new(),
            frecency: Frecency::default(),
            acceptance,
            frecency_loaded: false,
            history_source: None,
            history: Arc::default(),
            generator_session: crate::generate::GeneratorSession::default(),
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
        let registry = Self::load_registry(&specs_dir)?;
        let mut engine = Self::from_registry(&specs_dir, registry, acceptance);
        engine.frecency = frecency;
        engine.frecency_loaded = true;
        // `new_with_frecency` is the test/embedding constructor that
        // intentionally supplies its own ranking data. Treat it as the
        // default source until a request asks for a different shell or
        // history setting.
        engine.history_source = Some(rank::HistorySourceConfig {
            custom_command: None,
            all_shells: false,
            current_shell: rank::HistoryShell::Unknown,
        });
        Ok(engine)
    }

    fn rebind_hosts(&mut self, registry: &Registry) {
        self.native = Arc::new(NativeHooks::load(&self.specs_dir, registry.snapshot().as_ref()));
        self.hook_cache = crate::hook_cache::HookCache::new();
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The WebView's `clear-cache` event (`ftab hook clear-autocomplete-cache`):
    /// `resetCaches()` dropped every loaded and generated spec, and
    /// `generatorCache.clear()` every generator result. Re-index the specs
    /// directory so a spec edited under `devCompletionsFolder` or
    /// `~/.fig/autocomplete/build` is read again, and forget every hook
    /// result, the debounce session and the history argument index built on
    /// the old specs.
    pub fn clear_caches(&mut self) {
        let _ = self.clear_caches_and_report();
    }

    /// Same reset as [`Self::clear_caches`], with an indication that the
    /// canonical directory was successfully reopened. The worker uses this
    /// to avoid discarding its last good registry during an install rename
    /// window.
    pub(crate) fn clear_caches_and_report(&mut self) -> bool {
        self.hook_cache.clear();
        self.generator_session = crate::generate::GeneratorSession::default();
        self.history = Arc::default();
        match Self::load_registry(&self.specs_dir) {
            Ok(registry) => {
                // A successful reload is a new generation. Rebind both hosts
                // together with the Registry so hooks and lazy specs cannot
                // observe different trees.
                self.rebind_hosts(&registry);
                self.registry = registry;
                true
            },
            Err(err) => {
                tracing::warn!(%err, "clear-cache: specs directory could not be re-indexed");
                false
            },
        }
    }

    fn refresh_specs_generation(&mut self) {
        if !self.registry.needs_refresh() {
            return;
        }
        match Self::load_registry(&self.specs_dir) {
            Ok(registry) => {
                self.rebind_hosts(&registry);
                self.registry = registry;
                // GeneratorSession lives outside the Registry/hosts and can
                // otherwise replay a custom result from the prior generation.
                self.generator_session = crate::generate::GeneratorSession::default();
                self.history = Arc::default();
            },
            Err(error) => {
                // The publisher may currently have the canonical path
                // absent. Keep the last generation for this request; the next
                // request retries the complete Registry+host rebuild.
                tracing::debug!(%error, "spec generation refresh deferred");
            },
        }
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

    fn ensure_frecency(&mut self, request: &CompleteRequest) {
        let custom_command = fig_settings::settings::get_string_or("beta.history.customCommand", String::new());
        let custom_command = (!custom_command.is_empty()).then_some(custom_command);
        let all_shells = fig_settings::settings::get_bool_or("beta.history.allShells", false);
        let source = rank::history_source_config(
            custom_command,
            all_shells,
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

    pub fn complete(&mut self, request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        self.refresh_specs_generation();
        crate::generate::install_session(std::mem::take(&mut self.generator_session));
        let result = self.complete_with_thread_session(request);
        self.generator_session = crate::generate::take_session();
        result
    }

    fn complete_with_thread_session(&mut self, mut request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        let buffer = lookup::completion_buffer(&request.buffer, request.cursor);
        let (tokens, ends_with_space) = lookup::tokenize(buffer);

        let history_disabled = fig_settings::settings::get_bool_or("autocomplete.history.disableLoading", false);
        if history_loading_enabled(history_disabled) {
            self.ensure_frecency(&request);
        }
        let lines = self.frecency.command_lines();
        if !Arc::ptr_eq(self.history.lines(), &lines) {
            self.history = Arc::new(crate::history::HistoryStore::new(lines));
        }
        crate::generate::set_history(Arc::clone(&self.history));
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
                    ..CompleteResult::default()
                });
            }
            let prefix = rank::history_prefix_from_buffer(buffer, ends_with_space, &tokens);
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
                ..CompleteResult::default()
            });
        }
        let mut result = {
            let _native = hook_backend::bind_native(Arc::clone(&self.native));
            let shell = ShellContext {
                current_process: request.current_process.clone().unwrap_or_default(),
                environment_variables: std::mem::take(&mut request.environment_variables),
            };
            let registry = &mut self.registry;
            let _cache = self.hook_cache.bind();
            hook_backend::enter_context(&request.cwd, &shell, || lookup::complete(registry, &request))
        };
        if should_merge_history(request.include_history, history_disabled) {
            let effective_fuzzy = result.fuzzy;
            let prefix = rank::history_prefix_from_buffer(buffer, ends_with_space, &tokens);
            rank::merge_history_with_prefix(&mut result, &tokens, prefix, &self.frecency, effective_fuzzy);
        }
        let alphabetical =
            fig_settings::settings::get_string_or("autocomplete.sortMethod", "default".into()) == "alphabetical";
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

/// Fig `importSpecFromLocation`: `devCompletionsFolder` is consulted first,
/// and only while `isInDevMode()` (`autocomplete.developerMode` or
/// `autocomplete.developerModeNPM`); `~/.fig/autocomplete/build` is reached
/// only when `publicSpecExists(name)` is false, so it never shadows a bundled
/// spec.
fn overlay_local_spec_dirs(registry: &mut Registry) {
    let dev_mode = fig_settings::settings::get_bool_or("autocomplete.developerMode", false)
        || fig_settings::settings::get_bool_or("autocomplete.developerModeNPM", false);
    if dev_mode
        && let Ok(Some(dev)) = fig_settings::settings::get_string("autocomplete.devCompletionsFolder")
        && !dev.is_empty()
    {
        registry.overlay_specs_dir(std::path::Path::new(&dev), crate::ir::OverlayMode::Replace);
    }
    if let Some(home) = std::env::var_os("HOME") {
        registry.overlay_specs_dir(
            &std::path::PathBuf::from(home).join(".fig/autocomplete/build"),
            crate::ir::OverlayMode::FillMissing,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::fs;

    #[derive(Debug, Deserialize)]
    struct Phase1Golden {
        cases: Vec<Phase1GoldenCase>,
    }

    #[derive(Debug, Deserialize)]
    struct Phase1GoldenCase {
        name: String,
        request: CompleteRequest,
        result: serde_json::Value,
    }

    fn write_spec(dir: &std::path::Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.json")), body).unwrap();
    }

    #[test]
    fn missing_specs_directory_fails_closed_instead_of_becoming_empty_registry() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("specs-ir");

        let error = Engine::load_registry(&missing).expect_err("missing IR must not load as empty");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("specs IR directory is unavailable"), "{rendered}");
        assert!(rendered.contains("specs-ir"), "{rendered}");
    }

    #[test]
    fn existing_empty_directory_remains_a_legacy_fixture() {
        let root = tempfile::tempdir().unwrap();

        let registry = Engine::load_registry(root.path()).expect("an existing fixture directory is valid");
        assert!(registry.is_empty());
    }

    fn write_typed_custom(dir: &std::path::Path, hook_id: &str, name: &str) {
        let (id, entry) = crate::hook_backend::test_typed_entry(
            hook_id,
            "custom",
            serde_json::json!({
                "op": "array",
                "items": [{
                    "op": "object",
                    "fields": [{"key": "name", "value": {"op": "string", "value": name}}]
                }]
            }),
        );
        let catalog = serde_json::json!({
            "version": 1,
            "kind": "typed-hook-expressions",
            "contracts": crate::hook_backend::test_sidecar_contracts(),
            "hooks": { id: entry }
        });
        fs::write(dir.join("typed-hooks.json"), format!("{catalog}\n")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn engine_rejects_cross_generation_lazy_reads_and_rebinds_after_publish() {
        fn write_generation(dir: &std::path::Path, child_description: &str, hook_name: &str) {
            write_spec(
                dir,
                "demo",
                &serde_json::json!({
                    "names": ["demo"],
                    "description": child_description,
                    "subcommands": [{"names": ["child"], "loadSpec": "nested"}],
                    "args": [{"jsCustom": "demo#custom#0"}]
                })
                .to_string(),
            );
            write_spec(
                dir,
                "nested",
                &serde_json::json!({
                    "names": ["nested"],
                    "description": child_description
                })
                .to_string(),
            );
            fs::write(dir.join("index.json"), r#"{"files":{"demo":"demo.json"}}"#).unwrap();
            write_typed_custom(dir, "demo#custom#0", hook_name);
        }

        let root = tempfile::tempdir().unwrap();
        let generation_a = root.path().join("generation-a");
        let generation_b = root.path().join("generation-b");
        write_generation(&generation_a, "from generation A", "from-A");
        write_generation(&generation_b, "from generation B", "from-B");

        let canonical = root.path().join("specs-ir");
        let backup = root.path().join("specs-ir.backup");
        fs::rename(&generation_a, &canonical).unwrap();
        let mut engine = Engine::new(canonical.clone()).expect("generation A engine");

        fs::rename(&canonical, &backup).unwrap();
        fs::rename(&generation_b, &canonical).unwrap();

        assert!(engine.registry.get_arc("demo").is_none());
        let stale = {
            let _bound = crate::hook_backend::bind_native(std::sync::Arc::clone(&engine.native));
            crate::hook_backend::dispatch_custom(
                "demo#custom#0",
                &[],
                "/tmp",
                "",
                std::time::Duration::from_secs(1),
                false,
            )
        };
        assert_eq!(
            stale.expect("stale native catalog stays generation A")[0].name,
            "from-A"
        );

        let result = engine
            .complete(CompleteRequest {
                buffer: "demo ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("stable generation B");
        assert!(
            result.suggestions.iter().any(|suggestion| suggestion.name == "from-B"),
            "{result:?}"
        );

        let generation_c = root.path().join("generation-c");
        write_generation(&generation_c, "from generation C", "from-C");
        let backup_b = root.path().join("specs-ir.backup-b");
        fs::rename(&canonical, &backup_b).unwrap();
        let during_gap = engine
            .complete(CompleteRequest {
                buffer: "demo ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("gap should not poison the old engine");
        assert!(
            during_gap
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "from-B"),
            "{during_gap:?}"
        );
        fs::rename(&generation_c, &canonical).unwrap();
        fs::remove_dir_all(&backup_b).unwrap();
        let recovered = engine
            .complete(CompleteRequest {
                buffer: "demo ".into(),
                cwd: "/tmp".into(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("generation C should be retried");
        assert!(
            recovered
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "from-C"),
            "{recovered:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn identical_file_content_can_be_read_across_a_generation_replacement() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        for dir in [&a, &b] {
            write_spec(
                dir,
                "demo",
                r#"{"names":["demo"],"args":[{"jsCustom":"demo#custom#0"}]}"#,
            );
            fs::write(dir.join("index.json"), r#"{"files":{"demo":"demo.json"}}"#).unwrap();
            write_typed_custom(dir, "demo#custom#0", "same");
        }
        let canonical = root.path().join("specs-ir");
        let backup = root.path().join("backup");
        fs::rename(&a, &canonical).unwrap();
        let mut engine = Engine::new(canonical.clone()).unwrap();
        fs::rename(&canonical, &backup).unwrap();
        fs::rename(&b, &canonical).unwrap();

        let demo = engine.registry.get_arc("demo").expect("same spec bytes");
        assert_eq!(demo.names, vec!["demo"]);
        let hook = {
            let _bound = crate::hook_backend::bind_native(std::sync::Arc::clone(&engine.native));
            crate::hook_backend::dispatch_custom(
                "demo#custom#0",
                &[],
                "/tmp",
                "",
                std::time::Duration::from_secs(1),
                false,
            )
        };
        assert_eq!(hook.expect("same catalog bytes")[0].name, "same");
        fs::remove_dir_all(backup).unwrap();
        let _ = engine.complete(CompleteRequest {
            buffer: "demo ".into(),
            cwd: "/tmp".into(),
            include_history: false,
            ..CompleteRequest::default()
        });
    }

    #[test]
    fn generate_spec_merges_dynamic_subcommands() {
        let _lock = engine_lock();
        let dir = tempfile::tempdir().unwrap();
        let (id, entry) = crate::hook_backend::test_typed_entry(
            "php#generateSpec#0",
            "generateSpec",
            serde_json::json!({
                "op": "spec-object",
                "fields": [
                    {"key": "name", "value": {"op": "string", "value": "php"}},
                    {
                        "key": "subcommands",
                        "value": {
                            "op": "array",
                            "items": [{
                                "op": "spec-object",
                                "fields": [
                                    {"key": "name", "value": {"op": "string", "value": "artisan"}},
                                    {"key": "description", "value": {"op": "string", "value": "Laravel"}}
                                ]
                            }]
                        }
                    }
                ]
            }),
        );
        let catalog = serde_json::json!({
            "version": 1,
            "kind": "typed-hook-expressions",
            "contracts": crate::hook_backend::test_sidecar_contracts(),
            "hooks": { id: entry }
        });
        fs::write(dir.path().join("typed-hooks.json"), format!("{catalog}\n")).unwrap();
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
                include_history: false,
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
    fn phase1_static_ir_complete_result_golden() {
        let _lock = engine_lock();
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/phase1");
        let golden: Phase1Golden = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/testdata/phase1/expected.json"
        )))
        .expect("phase1 golden JSON");
        assert!(!golden.cases.is_empty(), "phase1 golden must contain cases");
        let settings = fig_settings::settings::Settings::from_slice(&[
            ("autocomplete.disableForCommands", serde_json::json!([])),
            ("autocomplete.hideAutoExecuteSuggestion", serde_json::json!(true)),
        ]);
        let mut registry = Registry::load(&fixture_dir).expect("phase1 static IR");

        for case in golden.cases {
            let mut result = lookup::complete_with_settings(&mut registry, &case.request, &settings);
            let (tokens, _) = lookup::tokenize(lookup::completion_buffer(&case.request.buffer, case.request.cursor));
            rank::apply_with_acceptance(
                &mut result,
                &tokens,
                &Frecency::default(),
                &rank::AcceptanceIndex::default(),
                &ranking_root_command(&case.request.buffer, case.request.cursor),
                false,
            );
            let actual = serde_json::to_value(&result).expect("serialize phase1 result");
            assert_eq!(actual, case.result, "phase1 golden case {:?}", case.name);
        }
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
    fn unknown_command_does_not_run_cobra_complete() {
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
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(result.suggestions.is_empty(), "{:?}", result.suggestions);
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
                 "script":["printf","generated\\n"],"splitOn":"\n"}
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
        // An exact match may also get an auto-execute wrapper; that follows
        // `autocomplete.hideAutoExecuteSuggestion` on disk, so look at the
        // command row itself.
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
