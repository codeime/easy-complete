//! Static Fig-spec IR loaded from build-time JSON (no V8).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Builtin {
    GitRefs,
    GitBranches,
    GitTags,
    GitCommits,
    GitRemotes,
    GitChangedFiles,
    GitStashes,
    GitAliases,
    NpmScripts,
    NpmDeps,
    Cobra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Template {
    Filepaths,
    Folders,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterStrategy {
    Prefix,
    Fuzzy,
    Default,
}

impl FilterStrategy {
    pub fn effective_fuzzy(self, user_fuzzy: bool) -> bool {
        match self {
            Self::Prefix => false,
            Self::Fuzzy => true,
            Self::Default => user_fuzzy,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SuggestionMeta {
    /// Explicit Fig suggestion type.  The surrounding collection supplies a
    /// default (`arg`, `subcommand`, or `option`), but static rows may override
    /// it with values such as `file`, `folder`, or `special`.
    #[serde(default, rename = "type", alias = "kind")]
    pub suggestion_type: Option<String>,
    /// Type of the row before a wrapper such as auto-execute changed it.
    #[serde(default, alias = "originalType")]
    pub original_type: Option<String>,
    /// String form of Fig's getQueryTerm.  Function forms are intentionally
    /// omitted by the compiler because they cannot run in the native engine.
    #[serde(default, alias = "getQueryTerm")]
    pub get_query_term: Option<String>,
    /// Explicit text to put in the shell buffer.  Fig calls this `insertValue`.
    #[serde(default, alias = "insertValue")]
    pub insert_value: Option<String>,
    /// Text shown in the list while keeping `insert_value` as the accepted text.
    #[serde(default, alias = "displayName")]
    pub display_name: Option<String>,
    /// Separator to append before the cursor (for example `=` for an option).
    #[serde(default, alias = "separatorToAdd")]
    pub separator_to_add: Option<String>,
    /// Explicitly override the automatic trailing-space heuristic.
    #[serde(default, alias = "shouldAddSpace")]
    pub should_add_space: Option<bool>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub priority: Option<i64>,
    /// Fig icon URI/emoji marker.  The desktop layer may resolve this to an image.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default, alias = "isDangerous")]
    pub is_dangerous: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SuggestionSeed {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Preformatted display hint for static suggestion arguments. Dynamic JS
    /// argument objects are intentionally not retained in the native IR.
    #[serde(default, alias = "argsHint")]
    pub args_hint: String,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
}

/// A static reference to another Fig spec.  String references are resolved by
/// `Registry` from the bundled JSON files.  Inline objects are retained for
/// forward compatibility with argument/option `loadSpec` values; node-level
/// inline objects are normally flattened by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum LoadSpec {
    Path(String),
    Inline(Box<Spec>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArgSpec {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub templates: Vec<Template>,
    #[serde(default)]
    pub script: Vec<String>,
    /// Fig `splitOn` separator. When present the native runner splits script
    /// stdout on this string instead of assuming newlines. A JS `postProcess`
    /// hook, when present, runs instead of this split.
    #[serde(default, alias = "splitOn")]
    pub split_on: Option<String>,
    /// Extracted Fig `postProcess` hook id. The worker looks up source under
    /// `hooks/` and runs it in the process-local QuickJS runtime.
    #[serde(default, alias = "jsPostProcess")]
    pub js_post_process: Option<String>,
    /// Extracted Fig `custom` generator hook id.
    #[serde(default, alias = "jsCustom")]
    pub js_custom: Option<String>,
    /// Extracted function-form `script` hook id. Returns argv / a command object.
    #[serde(default, alias = "jsScript")]
    pub js_script: Option<String>,
    /// Fig generator `cache.cacheKey`. Combined with tokens/cwd in the Rust SWR cache.
    #[serde(default, alias = "cacheKey")]
    pub cache_key: Option<String>,
    #[serde(default, alias = "cacheByDirectory")]
    pub cache_by_directory: Option<bool>,
    #[serde(default, alias = "cacheTtl")]
    pub cache_ttl_ms: Option<i64>,
    /// Generator-level `scriptTimeout`, in milliseconds.  The compiler keeps
    /// this alongside the static script/builtin it selected so the native
    /// runner can preserve Fig's max(default, generator, command) rule.
    #[serde(default, alias = "scriptTimeout")]
    pub script_timeout_ms: Option<i64>,
    #[serde(default)]
    pub builtin: Option<Builtin>,
    /// Some Fig arguments combine several generators (for example checkout
    /// offers branches, tags and paths).  Keep every native generator instead
    /// of silently replacing all but the last one during compilation.
    #[serde(default)]
    pub builtins: Vec<Builtin>,
    #[serde(default)]
    pub suggestions: Vec<SuggestionSeed>,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
    #[serde(default, alias = "loadSpec")]
    pub load_spec: Option<LoadSpec>,
    /// Resolved static argument spec.  Keep the source `load_spec` in the
    /// deserialized IR (so dynamic/unsupported forms remain observable), but
    /// expose a native-ready tree for the lookup state machine without
    /// serializing a second copy back into the bundle. `Arc` so a walk can
    /// enter the loaded node without cloning the tree.
    #[serde(skip)]
    pub resolved_spec: Option<Arc<Spec>>,
    #[serde(default, alias = "isOptional")]
    pub is_optional: bool,
    #[serde(default, alias = "isVariadic")]
    pub is_variadic: bool,
    #[serde(default, alias = "filterStrategy")]
    pub filter_strategy: Option<FilterStrategy>,
    /// Per-argument override for the global always-suggest-current-token
    /// setting. `None` means inherit the setting; `Some(false)` is explicit.
    #[serde(default, rename = "suggestCurrentToken", alias = "suggest_current_token")]
    pub suggest_current_token: Option<bool>,
    /// When a variadic argument has already consumed a value, this controls
    /// whether a following option may start a new option context.
    #[serde(default, alias = "optionsCanBreakVariadicArg")]
    pub options_can_break_variadic_arg: Option<bool>,
    /// Completed token loads that command's spec from the registry, matching
    /// Fig `isCommand`. In-progress tokens reuse the first-token command list.
    #[serde(default, alias = "isCommand")]
    pub is_command: bool,
    /// Like `is_command`, but path tokens resolve by basename (Fig `isScript`).
    #[serde(default, alias = "isScript")]
    pub is_script: bool,
    /// Prefix concatenated with the token to form a global spec name
    /// (`python/` + `http` → `python/http`).
    #[serde(default, alias = "isModule")]
    pub is_module: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParserDirectives {
    /// If true, options stop being consumable after a positional argument has
    /// been entered in the current completion object.
    #[serde(default, alias = "optionsMustPrecedeArguments")]
    pub options_must_precede_arguments: Option<bool>,
    /// Treat a single-dash token such as `-foo` as a long option rather than a
    /// POSIX short-option chain.
    #[serde(default, alias = "flagsArePosixNoncompliant")]
    pub flags_are_posix_noncompliant: Option<bool>,
    /// Separators used by attached option values (`--foo:value`). An explicit
    /// empty array intentionally disables attached separators.
    #[serde(default, alias = "optionArgSeparators")]
    pub option_arg_separators: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptionSpec {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Shared like [`Spec::args`]: walk into `--opt value` clones a pointer,
    /// not the option's generator tree.
    #[serde(default)]
    pub args: Vec<Arc<ArgSpec>>,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
    #[serde(default, alias = "loadSpec")]
    pub load_spec: Option<LoadSpec>,
    /// A boolean `requiresSeparator` uses the command's default separator (`=`)
    /// while a string value is preserved by the compiler in `separator_to_add`.
    #[serde(default, alias = "requiresSeparator")]
    pub requires_separator: Option<serde_json::Value>,
    #[serde(default, alias = "requiresEquals")]
    pub requires_equals: bool,
    #[serde(default, alias = "requiresSubcommand")]
    pub requires_subcommand: Option<bool>,
    /// Options that become unavailable after this option is passed. Values
    /// use the same alias spellings as Fig's `exclusiveOn` field.
    #[serde(default, alias = "exclusiveOn")]
    pub exclusive_on: Vec<String>,
    /// Options whose rows should be promoted while one of these dependencies
    /// is still unmet. The WebView uses priority 75 for these rows.
    #[serde(default, alias = "dependsOn")]
    pub depends_on: Vec<String>,
    /// `false`/omitted means once, `true` means unlimited, and a number is the
    /// maximum number of times the option may be passed.
    #[serde(default, alias = "isRepeatable")]
    pub is_repeatable: Option<serde_json::Value>,
    /// Persistent options are copied into child subcommands by Fig's parser.
    /// The compiler keeps this marker on the option while `Spec` stores the
    /// current effective persistent set separately.
    #[serde(default, alias = "isPersistent")]
    pub is_persistent: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Spec {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Shared so listing and walk clone pointers, not the nested tree.
    #[serde(default)]
    pub subcommands: Vec<Arc<Spec>>,
    /// Shared so listing and walk clone pointers, not each option's arg tree.
    /// `gcc -` / `clang -` / `curl -` would otherwise deep-copy hundreds of
    /// `OptionSpec`s on every keystroke just to override listing priority.
    #[serde(default)]
    pub options: Vec<Arc<OptionSpec>>,
    /// Effective persistent options for this node. For a lazy `loadSpec`, the
    /// lookup walker merges the parent set into this set as it descends.
    #[serde(default, alias = "persistentOptions")]
    pub persistent_options: Vec<Arc<OptionSpec>>,
    /// Shared so walk and listing clone pointers, not generator trees
    /// (templates, scripts, suggestion seeds, JS hook ids).
    #[serde(default)]
    pub args: Vec<Arc<ArgSpec>>,
    #[serde(default, alias = "additionalSuggestions")]
    pub additional_suggestions: Vec<SuggestionSeed>,
    #[serde(flatten)]
    pub meta: SuggestionMeta,
    #[serde(default, alias = "loadSpec")]
    pub load_spec: Option<LoadSpec>,
    #[serde(default, alias = "requiresSubcommand")]
    pub requires_subcommand: Option<bool>,
    #[serde(default, alias = "filterStrategy")]
    pub filter_strategy: Option<FilterStrategy>,
    #[serde(default, alias = "parserDirectives")]
    pub parser_directives: Option<ParserDirectives>,
    /// Extracted Fig `generateSpec` hook id. Walk merges the returned tree
    /// into this node, keeping the wrapper names.
    #[serde(default, alias = "jsGenerateSpec")]
    pub js_generate_spec: Option<String>,
    #[serde(default, alias = "generateSpecCacheKey")]
    pub generate_spec_cache_key: Option<String>,
}

impl Spec {
    pub fn has_name(&self, name: &str) -> bool {
        self.names.iter().any(|candidate| candidate == name)
    }

    pub fn find_subcommand(&self, name: &str) -> Option<&Spec> {
        self.subcommands
            .iter()
            .find(|spec| spec.has_name(name))
            .map(Arc::as_ref)
    }
}

#[derive(Debug, Default, Clone)]
pub struct Registry {
    specs: HashMap<String, Arc<Spec>>,
    files: HashMap<Arc<str>, PathBuf>,
    /// Root directory is retained so a node's `loadSpec: "foo/bar"` can be
    /// resolved without adding every implementation path to command names.
    root: PathBuf,
    /// New compiler output supplies command/alias mappings in index.json.  In
    /// that mode nested implementation files stay private; old indexes keep
    /// the historical relative-path fallback in `index_dir`.
    has_command_file_map: bool,
    /// Case-insensitive sorted command names; `Arc` shares the `files` keys.
    names: Vec<Arc<str>>,
    /// LRU of loaded spec trees (one entry per file, not per alias).
    loaded: VecDeque<Arc<Spec>>,
}

const MAX_CACHED_SPECS: usize = 48;
const MAX_NAME_MATCHES: usize = 50;

/// Max-heap by ignore-ASCII-case so we can keep the 50 alphabetically first fuzzy hits.
struct AlphaMax<'a>(&'a str);

impl PartialEq for AlphaMax<'_> {
    fn eq(&self, other: &Self) -> bool {
        crate::query::cmp_ignore_ascii_case(self.0, other.0) == Ordering::Equal
    }
}

impl Eq for AlphaMax<'_> {}

impl PartialOrd for AlphaMax<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AlphaMax<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        crate::query::cmp_ignore_ascii_case(self.0, other.0)
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, spec: Spec) {
        self.insert_loaded(spec, None);
    }

    fn insert_loaded(&mut self, spec: Spec, path: Option<&Path>) {
        let spec = Arc::new(spec);
        for name in &spec.names {
            if name.is_empty() {
                continue;
            }
            if let Some(path) = path {
                if self.has_command_file_map && !self.files.contains_key(name.as_str()) {
                    continue;
                }
                if self
                    .files
                    .get(name.as_str())
                    .is_some_and(|existing| existing.as_path() != path)
                {
                    continue;
                }
            }
            self.specs.insert(name.clone(), spec.clone());
            self.remember_name(Arc::<str>::from(name.as_str()));
        }
        self.loaded.push_back(spec);
    }

    fn remember_name(&mut self, name: Arc<str>) {
        if name.is_empty() {
            return;
        }
        let idx = self
            .names
            .partition_point(|existing| crate::query::cmp_ignore_ascii_case(existing, &name).is_lt());
        if self
            .names
            .get(idx)
            .is_some_and(|existing| existing.eq_ignore_ascii_case(&name))
        {
            return;
        }
        self.names.insert(idx, name);
    }

    fn remember_file(&mut self, name: String, path: PathBuf) {
        let name: Arc<str> = name.into();
        if let std::collections::hash_map::Entry::Vacant(entry) = self.files.entry(name.clone()) {
            entry.insert(path);
            self.remember_name(name);
        }
    }

    fn rebuild_names(&mut self) {
        let mut names: Vec<Arc<str>> = self.files.keys().cloned().collect();
        names.sort_by(|a, b| crate::query::cmp_ignore_ascii_case(a, b));
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        self.names = names;
    }

    fn touch_loaded(&mut self, spec: &Arc<Spec>) {
        let Some(pos) = self.loaded.iter().position(|cached| Arc::ptr_eq(cached, spec)) else {
            return;
        };
        if pos + 1 == self.loaded.len() {
            return;
        }
        if let Some(cached) = self.loaded.remove(pos) {
            self.loaded.push_back(cached);
        }
    }

    fn evict_oldest_spec(&mut self) {
        while let Some(old) = self.loaded.pop_front() {
            let before = self.specs.len();
            self.specs.retain(|_, cached| !Arc::ptr_eq(cached, &old));
            if self.specs.len() < before {
                return;
            }
        }
    }

    fn ensure_loaded(&mut self, name: &str) {
        if let Some(spec) = self.specs.get(name).cloned() {
            self.touch_loaded(&spec);
            return;
        }
        let Some(path) = self.files.get(name).cloned() else {
            return;
        };
        let root = self.root.clone();
        let files = self.files.clone();
        if let Ok(mut spec) = load_spec_file(&path, &root, &files, &mut Vec::new()) {
            if self.loaded.len() >= MAX_CACHED_SPECS {
                self.evict_oldest_spec();
            }
            if !spec.names.iter().any(|candidate| candidate == name) {
                spec.names.push(name.to_string());
            }
            if !self.has_command_file_map {
                for alias in &spec.names {
                    if !alias.is_empty() {
                        self.remember_file(alias.clone(), path.clone());
                    }
                }
            }
            self.insert_loaded(spec, Some(&path));
        }
    }

    pub fn get(&mut self, name: &str) -> Option<&Spec> {
        self.ensure_loaded(name);
        self.specs.get(name).map(Arc::as_ref)
    }

    /// Same lookup as [`Self::get`], but the caller can keep the spec after
    /// the next mutable registry operation (for example an `isCommand` switch).
    pub fn get_arc(&mut self, name: &str) -> Option<Arc<Spec>> {
        self.ensure_loaded(name);
        self.specs.get(name).cloned()
    }

    pub fn command_names_matching(&self, query: &str) -> Vec<(String, String)> {
        self.command_names_matching_with(query, false)
    }

    /// First-token completion mirrors the shell command generator, which
    /// keeps the exact current command in the result so the legacy
    /// auto-execute wrapper can offer Enter on an already-complete token.
    /// The normal command-name lookup intentionally omits exact matches for
    /// subcommand-style completion, so expose this narrow variant instead of
    /// changing that established behavior.
    pub fn command_names_matching_including_exact_with(&self, query: &str, fuzzy: bool) -> Vec<(String, String)> {
        let mut matches = self.command_names_matching_with(query, fuzzy);
        if !query.is_empty()
            && let Some(name) = self.names.iter().find(|name| name.eq_ignore_ascii_case(query))
        {
            matches.insert(0, (name.to_string(), String::new()));
        }
        matches
    }

    pub fn command_names_matching_with(&self, query: &str, fuzzy: bool) -> Vec<(String, String)> {
        if query.is_empty() {
            return self
                .names
                .iter()
                .take(MAX_NAME_MATCHES)
                .map(|name| (name.to_string(), String::new()))
                .collect();
        }
        if fuzzy {
            let mut heap = BinaryHeap::with_capacity(MAX_NAME_MATCHES + 1);
            for name in &self.names {
                if name.eq_ignore_ascii_case(query) || !crate::query::matches_query(name, query, true) {
                    continue;
                }
                heap.push(AlphaMax(name.as_ref()));
                if heap.len() > MAX_NAME_MATCHES {
                    heap.pop();
                }
            }
            let mut matched: Vec<&str> = heap.into_iter().map(|item| item.0).collect();
            matched.sort_by(|a, b| crate::query::cmp_ignore_ascii_case(a, b));
            return matched
                .into_iter()
                .map(|name| (name.to_string(), String::new()))
                .collect();
        }
        let start = self
            .names
            .partition_point(|name| crate::query::cmp_ignore_ascii_case(name, query).is_lt());
        let mut out = Vec::new();
        for name in &self.names[start..] {
            if !crate::query::starts_with_ignore_case(name, query) {
                break;
            }
            if name.eq_ignore_ascii_case(query) {
                continue;
            }
            out.push((name.to_string(), String::new()));
            if out.len() >= MAX_NAME_MATCHES {
                break;
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.files.len().max(self.specs.len())
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.specs.is_empty()
    }

    #[cfg(test)]
    fn loaded_spec_count(&self) -> usize {
        self.loaded.len()
    }

    #[cfg(test)]
    fn is_cached(&self, name: &str) -> bool {
        self.specs.contains_key(name)
    }

    pub fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref();
        if !dir.is_dir() {
            return Err(anyhow!("specs IR directory does not exist: {}", dir.display()));
        }
        let mut registry = Self::new();
        registry.root = dir.to_path_buf();
        registry.has_command_file_map = read_index(dir, &mut registry)?;
        index_dir(dir, dir, &mut registry)?;
        registry.rebuild_names();
        Ok(registry)
    }
}

#[derive(Debug, Default, Deserialize)]
struct IrIndex {
    files: Option<HashMap<String, String>>,
}

fn safe_index_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let path = root.join(path);
    path.is_file().then_some(path)
}

fn read_index(root: &Path, registry: &mut Registry) -> anyhow::Result<bool> {
    let path = root.join("index.json");
    if !path.is_file() {
        return Ok(false);
    }
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let index: IrIndex = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    let Some(files) = index.files else {
        return Ok(false);
    };
    for (command, relative) in files {
        if command.is_empty() {
            continue;
        }
        let Some(path) = safe_index_path(root, &relative) else {
            continue;
        };
        registry.remember_file(command, path);
    }
    // The presence of `files`, even when every entry is invalid, is
    // authoritative.  `Registry::load` must not fall back to recursively
    // exposing nested implementation files in that case.
    Ok(true)
}

fn index_dir(root: &Path, dir: &Path, registry: &mut Registry) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            index_dir(root, &path, registry)?;
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == "index.json" || !name.ends_with(".json") {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let key = rel.with_extension("").to_string_lossy().replace('\\', "/");
        if !key.is_empty() && key != "index" {
            // With the new command map, every command entry comes from
            // index.json.  All JSON paths are implementation details and are
            // resolved only when a node's loadSpec references them.  When the
            // field is absent, retain the historical relative-path fallback.
            if registry.has_command_file_map {
                continue;
            }
            let key: Arc<str> = key.into();
            registry.files.entry(key).or_insert(path);
        }
    }
    Ok(())
}

fn resolve_reference_path(root: &Path, files: &HashMap<Arc<str>, PathBuf>, reference: &str) -> Option<PathBuf> {
    let reference = reference.trim().trim_start_matches("./");
    if reference.is_empty() || reference.contains('\\') {
        return None;
    }
    if let Some(path) = files.get(reference) {
        return Some(path.clone());
    }
    let relative = if reference.ends_with(".json") {
        reference.to_string()
    } else {
        format!("{reference}.json")
    };
    safe_index_path(root, &relative)
}

fn replace_spec_with_loaded(base: &mut Spec, mut loaded: Spec) {
    // `loadSpec` in the JS parser replaces the current completion object.
    // Keep the wrapper names so a parent such as `chezmoi git` or `pass grep`
    // still resolves the node by the spelling present in the command line.
    if !base.names.is_empty() {
        loaded.names = base.names.clone();
    }
    *base = loaded;
}

fn resolve_spec_references(spec: &mut Spec, root: &Path, files: &HashMap<Arc<str>, PathBuf>, stack: &mut Vec<PathBuf>) {
    let load_spec = spec.load_spec.take();
    if let Some(load_spec) = load_spec {
        let target = match load_spec {
            LoadSpec::Path(reference) => resolve_reference_path(root, files, &reference),
            LoadSpec::Inline(target) => {
                replace_spec_with_loaded(spec, *target);
                None
            },
        };
        if let Some(target_path) = target {
            let already_loading = stack.iter().any(|path| path == &target_path);
            if !already_loading {
                if let Ok(loaded) = load_spec_file_inner(&target_path, root, files, stack) {
                    replace_spec_with_loaded(spec, loaded);
                }
            }
        }
    }

    for child in &mut spec.subcommands {
        resolve_spec_references(Arc::make_mut(child), root, files, stack);
    }

    for arg in &mut spec.args {
        resolve_arg_spec(Arc::make_mut(arg), root, files, stack);
    }
    for option in &mut spec.options {
        for arg in &mut Arc::make_mut(option).args {
            resolve_arg_spec(Arc::make_mut(arg), root, files, stack);
        }
    }
}

fn resolve_arg_spec(arg: &mut ArgSpec, root: &Path, files: &HashMap<Arc<str>, PathBuf>, stack: &mut Vec<PathBuf>) {
    let Some(load_spec) = arg.load_spec.as_ref() else {
        return;
    };

    match load_spec {
        LoadSpec::Path(reference) => {
            let Some(target_path) = resolve_reference_path(root, files, reference) else {
                return;
            };
            if stack.iter().any(|path| path == &target_path) {
                return;
            }
            if let Ok(loaded) = load_spec_file_inner(&target_path, root, files, stack) {
                arg.resolved_spec = Some(Arc::new(loaded));
            }
        },
        LoadSpec::Inline(target) => {
            let mut loaded = (**target).clone();
            resolve_spec_references(&mut loaded, root, files, stack);
            arg.resolved_spec = Some(Arc::new(loaded));
        },
    }
}

fn load_spec_file_inner(
    path: &Path,
    root: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<Spec> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut spec: Spec = serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    stack.push(path.to_path_buf());
    resolve_spec_references(&mut spec, root, files, stack);
    stack.pop();
    Ok(spec)
}

fn load_spec_file(
    path: &Path,
    root: &Path,
    files: &HashMap<Arc<str>, PathBuf>,
    stack: &mut Vec<PathBuf>,
) -> anyhow::Result<Spec> {
    load_spec_file_inner(path, root, files, stack)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_spec(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.json")), body).unwrap();
    }

    #[test]
    fn loads_git_fixture_subcommands() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names": ["git"],
              "description": "the stupid content tracker",
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["commit"], "description": "Record changes"},
                {"names": ["cherry-pick"], "description": "Apply commits"}
              ],
              "options": [{"names": ["--help"], "description": "Show help"}]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let git = registry.get("git").expect("git spec");
        assert!(git.find_subcommand("checkout").is_some());
        assert!(git.find_subcommand("cherry-pick").is_some());
        assert!(git.find_subcommand("status").is_none());
        assert_eq!(git.options[0].names, vec!["--help"]);
        let cloned = git.clone();
        assert!(
            Arc::ptr_eq(&cloned.subcommands[0], &git.subcommands[0]),
            "cloning a spec must share nested subcommand trees"
        );
        assert!(
            Arc::ptr_eq(&cloned.options[0], &git.options[0]),
            "cloning a spec must share option trees"
        );
    }

    #[test]
    fn args_are_shared_arcs() {
        let path = Arc::new(ArgSpec {
            name: "path".into(),
            templates: vec![Template::Folders],
            ..ArgSpec::default()
        });
        let mkdir = Spec {
            names: vec!["mkdir".into()],
            args: vec![Arc::clone(&path)],
            ..Spec::default()
        };
        assert!(std::ptr::eq(mkdir.args[0].as_ref(), path.as_ref()));
        assert!(Arc::ptr_eq(&mkdir.clone().args[0], &path));
    }

    #[test]
    fn nested_subcommands_are_shared_arcs() {
        let checkout = Arc::new(Spec {
            names: vec!["checkout".into()],
            description: "switch".into(),
            ..Spec::default()
        });
        let git = Spec {
            names: vec!["git".into()],
            subcommands: vec![Arc::clone(&checkout)],
            ..Spec::default()
        };
        let found = git.find_subcommand("checkout").expect("child");
        assert!(std::ptr::eq(found, checkout.as_ref()));
        assert!(Arc::ptr_eq(&git.clone().subcommands[0], &checkout));
    }

    #[test]
    fn options_are_shared_arcs() {
        let help = Arc::new(OptionSpec {
            names: vec!["--help".into()],
            description: "Show help".into(),
            ..OptionSpec::default()
        });
        let gcc = Spec {
            names: vec!["gcc".into()],
            options: vec![Arc::clone(&help)],
            ..Spec::default()
        };
        assert!(std::ptr::eq(gcc.options[0].as_ref(), help.as_ref()));
        assert!(Arc::ptr_eq(&gcc.clone().options[0], &help));
    }

    #[test]
    fn loads_mkdir_fixture_options_and_folder_template() {
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
                {"names": ["-v", "--verbose"], "description": "Print a message"}
              ]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let mkdir = registry.get("mkdir").expect("mkdir spec");
        assert_eq!(mkdir.args[0].templates, vec![Template::Folders]);
        assert!(mkdir.options.iter().any(|opt| opt.names.iter().any(|n| n == "-p")));
        let cloned = mkdir.clone();
        assert!(
            Arc::ptr_eq(&cloned.args[0], &mkdir.args[0]),
            "cloning a spec must share argument trees"
        );
    }

    #[test]
    fn command_names_matching_skips_exact_and_matches_prefix() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "git", r#"{"names":["git"],"description":"git"}"#);
        write_spec(dir.path(), "gzip", r#"{"names":["gzip"],"description":"gzip"}"#);
        let registry = Registry::load(dir.path()).expect("load");
        assert_eq!(registry.loaded_spec_count(), 0);
        let names: Vec<_> = registry
            .command_names_matching("gi")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["git"]);
        assert!(registry.command_names_matching("git").is_empty());
        assert_eq!(registry.loaded_spec_count(), 0);
        let gz: Vec<_> = registry
            .command_names_matching("gz")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(gz, vec!["gzip"]);
        assert_eq!(registry.loaded_spec_count(), 0);
    }

    #[test]
    fn spec_cache_evicts_oldest_not_all() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write_spec(dir.path(), &format!("cmd{i}"), &format!(r#"{{"names":["cmd{i}"]}}"#));
        }
        let mut registry = Registry::load(dir.path()).expect("load");
        for i in 0..48 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert_eq!(registry.loaded_spec_count(), 48);
        assert!(registry.is_cached("cmd0"));
        assert!(registry.get("cmd48").is_some());
        assert_eq!(registry.loaded_spec_count(), 48);
        assert!(!registry.is_cached("cmd0"));
        assert!(registry.is_cached("cmd1"));
        assert!(registry.is_cached("cmd48"));
        assert!(registry.get("cmd0").is_some());
        assert!(registry.is_cached("cmd0"));
        assert!(!registry.is_cached("cmd1"));
    }

    #[test]
    fn spec_cache_lru_keeps_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write_spec(dir.path(), &format!("cmd{i}"), &format!(r#"{{"names":["cmd{i}"]}}"#));
        }
        let mut registry = Registry::load(dir.path()).expect("load");
        for i in 0..48 {
            assert!(registry.get(&format!("cmd{i}")).is_some());
        }
        assert!(registry.get("cmd0").is_some());
        assert!(registry.get("cmd48").is_some());
        assert!(registry.is_cached("cmd0"));
        assert!(!registry.is_cached("cmd1"));
        assert!(registry.is_cached("cmd48"));
    }

    #[test]
    fn alias_does_not_overwrite_an_existing_spec_file() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "g",
            r#"{"names":["g"],"description":"the g tool","subcommands":[{"names":["only-in-g"]}]}"#,
        );
        write_spec(
            dir.path(),
            "git",
            r#"{"names":["git","g"],"description":"git","subcommands":[{"names":["checkout"]}]}"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let git = registry.get("git").expect("git");
        assert!(git.find_subcommand("checkout").is_some());
        let g = registry.get("g").expect("g");
        assert!(g.find_subcommand("only-in-g").is_some(), "{g:?}");
        assert!(g.find_subcommand("checkout").is_none());
    }

    #[test]
    fn fuzzy_name_match_keeps_alphabetically_first_fifty() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..60 {
            write_spec(
                dir.path(),
                &format!("test{i:02}"),
                &format!(r#"{{"names":["test{i:02}"]}}"#),
            );
        }
        let registry = Registry::load(dir.path()).expect("load");
        let names: Vec<_> = registry
            .command_names_matching_with("te", true)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names.len(), 50);
        assert_eq!(names[0], "test00");
        assert_eq!(names[49], "test49");
        assert!(!names.iter().any(|n| n == "test59"));
    }

    #[test]
    fn indexes_alias_names() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "npm",
            r#"{
              "names": ["npm"],
              "subcommands": [
                {"names": ["install", "i", "add"], "description": "Install a package"}
              ]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let npm = registry.get("npm").unwrap();
        assert!(npm.find_subcommand("i").is_some());
        assert!(npm.find_subcommand("add").is_some());
    }

    #[test]
    fn command_map_loads_versioned_alias_and_hides_nested_implementation_files() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "docker",
            r#"{
              "names":["docker"],
              "subcommands":[
                {"names":["compose"],"loadSpec":"docker-compose"},
                {"names":["domains"],"loadSpec":"gcloud/domains"}
              ]
            }"#,
        );
        write_spec(
            dir.path(),
            "docker-compose",
            r#"{"names":["docker-compose"],"subcommands":[{"names":["up"]}]}"#,
        );
        write_spec(
            dir.path(),
            "gcloud",
            r#"{"names":["gcloud"],"subcommands":[{"names":["domains"],"loadSpec":"gcloud/domains"}]}"#,
        );
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(
            dir.path().join("gcloud/domains.json"),
            r#"{"names":["domains"],"subcommands":[{"names":["list"]}]}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("heroku")).unwrap();
        fs::write(
            dir.path().join("heroku/8.0.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["old"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("heroku/8.6.0.json"),
            r#"{"names":["heroku"],"subcommands":[{"names":["new"]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{"completions":["docker","gcloud","heroku"],"files":{"docker":"docker.json","gcloud":"gcloud.json","heroku":"heroku/8.6.0.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let names: Vec<_> = registry
            .command_names_matching("")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.iter().any(|name| name == "heroku"));
        assert!(!names.iter().any(|name| name == "domains"));

        {
            let docker = registry.get("docker").expect("docker");
            let compose = docker.find_subcommand("compose").expect("compose");
            assert!(compose.find_subcommand("up").is_some());
            let domains = docker.find_subcommand("domains").expect("domains");
            assert!(domains.find_subcommand("list").is_some());
        }
        let heroku = registry.get("heroku").expect("heroku");
        assert!(heroku.find_subcommand("new").is_some());
        assert!(heroku.find_subcommand("old").is_none());
        assert!(!registry.names.iter().any(|name| name.as_ref() == "domains"));
    }

    #[test]
    fn command_map_keeps_canonical_paths_and_filters_unmapped_spec_aliases() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(dir.path(), "appwrite", r#"{"names":["index","internal"]}"#);
        write_spec(
            dir.path(),
            "autojump",
            r#"{"names":["autojump"],"description":"canonical"}"#,
        );
        write_spec(dir.path(), "j", r#"{"names":["autojump"],"description":"alias file"}"#);
        fs::write(
            dir.path().join("index.json"),
            r#"{
              "files": {
                "appwrite": "appwrite.json",
                "index": "appwrite.json",
                "autojump": "autojump.json",
                "j": "j.json"
              }
            }"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        {
            let appwrite = registry.get("appwrite").expect("appwrite");
            assert!(appwrite.has_name("appwrite"));
        }
        assert!(registry.get("internal").is_none());

        {
            let autojump = registry.get("autojump").expect("autojump");
            assert_eq!(autojump.description, "canonical");
        }
        let j = registry.get("j").expect("j");
        assert_eq!(j.description, "alias file");
    }

    #[test]
    fn invalid_command_map_does_not_fallback_to_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("gcloud")).unwrap();
        fs::write(dir.path().join("gcloud/domains.json"), r#"{"names":["domains"]}"#).unwrap();
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"gcloud":"gcloud/missing.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        assert!(registry.command_names_matching("").is_empty());
        assert!(registry.get("gcloud").is_none());
        assert!(registry.get("domains").is_none());
    }

    #[test]
    fn node_load_spec_replaces_wrapper_fields_for_pass_and_chezmoi() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "pass",
            r#"{
              "names":["pass"],
              "subcommands":[{
                "names":["grep"],
                "description":"wrapper description",
                "args":[{"name":"pass-name"}],
                "loadSpec":"grep"
              }]
            }"#,
        );
        write_spec(
            dir.path(),
            "grep",
            r#"{
              "names":["grep-target"],
              "description":"loaded description",
              "args":[{"name":"pattern"},{"name":"file"}]
            }"#,
        );
        write_spec(
            dir.path(),
            "chezmoi",
            r#"{
              "names":["chezmoi"],
              "subcommands":[{
                "names":["git"],
                "description":"wrapper description",
                "args":[{"name":"source-dir"}],
                "loadSpec":"git"
              }]
            }"#,
        );
        write_spec(
            dir.path(),
            "git",
            r#"{
              "names":["git-target"],
              "description":"loaded description",
              "args":[{"name":"command"}]
            }"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"pass":"pass.json","chezmoi":"chezmoi.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        {
            let pass = registry.get("pass").expect("pass");
            let grep = pass.find_subcommand("grep").expect("grep");
            assert_eq!(grep.names, vec!["grep"]);
            assert_eq!(grep.description, "loaded description");
            assert_eq!(
                grep.args.iter().map(|arg| arg.name.as_str()).collect::<Vec<_>>(),
                vec!["pattern", "file"]
            );
        }

        let chezmoi = registry.get("chezmoi").expect("chezmoi");
        let git = chezmoi.find_subcommand("git").expect("git");
        assert_eq!(git.names, vec!["git"]);
        assert_eq!(git.description, "loaded description");
        assert_eq!(
            git.args.iter().map(|arg| arg.name.as_str()).collect::<Vec<_>>(),
            vec!["command"]
        );
    }

    #[test]
    fn load_spec_cycle_and_missing_reference_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "a",
            r#"{"names":["a"],"subcommands":[{"names":["b"],"loadSpec":"b"},{"names":["missing"],"loadSpec":"not-present"}],"args":[{"name":"missing arg","loadSpec":"not-present"},{"name":"cycle arg","loadSpec":"b"}]}"#,
        );
        write_spec(
            dir.path(),
            "b",
            r#"{"names":["b"],"subcommands":[{"names":["a"],"loadSpec":"a"}],"args":[{"name":"cycle back","loadSpec":"a"}]}"#,
        );
        fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"a":"a.json","b":"b.json"}}"#,
        )
        .unwrap();

        let mut registry = Registry::load(dir.path()).expect("load");
        let a = registry.get("a").expect("a");
        let b = a.find_subcommand("b").expect("b");
        assert!(b.find_subcommand("a").is_some());
        assert!(a.find_subcommand("missing").is_some());
        assert!(a.args[0].resolved_spec.is_none());
        let cycle = a.args[1].resolved_spec.as_deref().expect("cycle target loads once");
        assert!(cycle.args[0].resolved_spec.is_none());
    }

    #[test]
    fn resolves_static_argument_load_specs_for_args_and_options() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "tool",
            r#"{
              "names":["tool"],
              "args":[{"name":"datasource","loadSpec":"arg-target"}],
              "options":[{"names":["--config"],"args":[{"name":"file","loadSpec":"option-target"}]}]
            }"#,
        );
        write_spec(
            dir.path(),
            "arg-target",
            r#"{"names":["arg-target"],"subcommands":[{"names":["list"]}]}"#,
        );
        write_spec(
            dir.path(),
            "option-target",
            r#"{"names":["option-target"],"subcommands":[{"names":["show"]}]}"#,
        );

        let mut registry = Registry::load(dir.path()).expect("load");
        let tool = registry.get("tool").expect("tool");
        let arg_spec = tool.args[0].resolved_spec.as_deref().expect("argument target");
        assert!(arg_spec.find_subcommand("list").is_some());
        let option_arg_spec = tool.options[0].args[0]
            .resolved_spec
            .as_deref()
            .expect("option argument target");
        assert!(option_arg_spec.find_subcommand("show").is_some());
        assert!(matches!(tool.args[0].load_spec, Some(LoadSpec::Path(ref path)) if path == "arg-target"));
    }

    #[test]
    fn resolves_inline_argument_load_spec_without_executing_code() {
        let dir = tempfile::tempdir().unwrap();
        write_spec(
            dir.path(),
            "dscl",
            r#"{
              "names":["dscl"],
              "args":[{"name":"datasource","loadSpec":{"names":["dscl"],"subcommands":[{"names":["list"]}]}}]
            }"#,
        );
        let mut registry = Registry::load(dir.path()).expect("load");
        let dscl = registry.get("dscl").expect("dscl");
        let Some(LoadSpec::Inline(loaded)) = dscl.args[0].load_spec.as_ref() else {
            panic!("expected inline loadSpec");
        };
        assert!(loaded.find_subcommand("list").is_some());
        let resolved = dscl.args[0].resolved_spec.as_deref().expect("resolved inline loadSpec");
        assert!(resolved.find_subcommand("list").is_some());
    }

    #[test]
    fn loaded_spec_lru_does_not_expect_the_position_index() {
        let src = include_str!("ir.rs");
        let start = src.find("fn touch_loaded").expect("touch_loaded");
        let body = &src[start..];
        let end = body.find("\n    fn evict_oldest_spec").expect("evict_oldest_spec");
        let body = &body[..end];
        assert!(
            !body.contains(".expect(") && body.contains("if let Some(cached) = self.loaded.remove(pos)"),
            "spec LRU reorder must not panic if the deque and map desync"
        );
    }
}
