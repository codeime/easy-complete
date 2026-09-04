//! Frecency ranking over spec suggestions, plus history command matches.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::query::MatchScore;
use crate::runtime::{CompleteResult, Suggestion};

/// Fig did not cap unique history rows. Keep a high ceiling so ranking cannot
/// drop a recent match the overlay would have shown, without walking an
/// unbounded database on every keystroke.
const HISTORY_LIMIT: usize = 10_000;
const CUSTOM_HISTORY_TIMEOUT: Duration = Duration::from_secs(5);
/// State key used for the native equivalent of the WebView's recency index.
/// The value is a JSON object `{ command: { acceptedName: unixMillis } }`.
pub const ACCEPTANCE_STATE_KEY: &str = "autocomplete.acceptanceRecency";

/// The shell families used by the legacy history source selector.  Keep this
/// deliberately small: Fig only loaded shell history for these three shells;
/// an unknown process fell back to the local database source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryShell {
    Zsh,
    Bash,
    Fish,
    Unknown,
}

/// Inputs which affect the history source.  Runtime keeps one of these beside
/// the frecency cache so changing settings or switching terminals reloads the
/// source once, rather than spawning a command for every keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistorySourceConfig {
    pub custom_command: Option<String>,
    pub all_shells: bool,
    pub current_shell: HistoryShell,
}

pub(crate) fn history_source_config(
    custom_command: Option<String>,
    all_shells: bool,
    current_shell: Option<&str>,
    current_process: Option<&str>,
) -> HistorySourceConfig {
    let shell = normalize_history_shell(current_shell);
    let current_shell = if shell != HistoryShell::Unknown {
        shell
    } else {
        normalize_history_shell(current_process)
    };
    HistorySourceConfig {
        custom_command,
        all_shells,
        current_shell,
    }
}

pub(crate) fn normalize_history_shell(value: Option<&str>) -> HistoryShell {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return HistoryShell::Unknown;
    };
    let value = value.strip_suffix(" (figterm)").unwrap_or(value);
    let value = value.rsplit('/').next().unwrap_or(value);
    match value {
        "zsh" => HistoryShell::Zsh,
        "bash" => HistoryShell::Bash,
        "fish" => HistoryShell::Fish,
        _ => HistoryShell::Unknown,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Frecency {
    /// command or `cmd sub` prefix -> (count, last_seen_unix_secs)
    stats: HashMap<String, (u32, u64)>,
    commands: Vec<(String, u64)>,
    /// The same commands as plain lines, shared with the `history` template
    /// generator. History is loaded once and read on every keystroke, so a
    /// per-request `Vec<String>` clone of ten thousand lines was the wrong
    /// shape; the thread-local takes a clone of this `Arc` instead.
    lines: Arc<Vec<String>>,
    /// `""` / `"git"` / `"git checkout"` → next-token occurrence counts.
    /// Built once when history is loaded so ranking does not walk the list
    /// on every keystroke.
    next_word_counts: HashMap<String, HashMap<String, usize>>,
}

impl Frecency {
    pub fn from_commands(commands: impl IntoIterator<Item = (String, u64)>) -> Self {
        let mut stats = HashMap::new();
        let mut stored = Vec::new();
        for (command, ts) in commands {
            let command = command.trim();
            if command.is_empty() {
                continue;
            }
            stored.push((command.to_string(), ts));
            bump(&mut stats, command, ts);
            let tokens: Vec<&str> = command.split_whitespace().collect();
            if let Some(first) = tokens.first() {
                bump(&mut stats, first, ts);
            }
            if tokens.len() >= 2 {
                bump(&mut stats, &format!("{} {}", tokens[0], tokens[1]), ts);
            }
        }
        let next_word_counts = next_word_counts(&stored);
        let lines = Arc::new(stored.iter().map(|(command, _)| command.clone()).collect());
        Self {
            stats,
            commands: stored,
            lines,
            next_word_counts,
        }
    }

    pub(crate) fn command_lines(&self) -> Arc<Vec<String>> {
        Arc::clone(&self.lines)
    }

    pub fn score(&self, key: &str) -> i64 {
        match self.stats.get(key) {
            Some((count, last)) => i64::from(*count) * 10_000 + i64::try_from(*last).unwrap_or(0) / 1_000,
            None => 0,
        }
    }

    pub fn history_suggestions(&self, query: &str, fuzzy: bool, include_empty_query: bool) -> Vec<Suggestion> {
        if query.is_empty() && !include_empty_query {
            return Vec::new();
        }
        // Count raw entries before query filtering or deduplication. Repeated
        // identical commands are what make the old history row more frequent.
        let first_word_counts = self.next_word_counts("");
        let mut seen = HashSet::new();
        self.commands
            .iter()
            .rev()
            .filter(|(command, _)| query.is_empty() || crate::query::matches_query(command, query, fuzzy))
            .filter_map(|(command, _)| {
                let command = command.trim_end();
                seen.insert(command.to_string()).then_some(command)
            })
            .take(HISTORY_LIMIT)
            .map(|command| {
                let priority = first_word_counts
                    .get(command.split_whitespace().next().unwrap_or_default())
                    .copied()
                    .map_or(50, history_priority_base);
                Suggestion::new(command, "past command", "history")
                    .with_insert_value(command)
                    .with_priority(priority)
            })
            .collect()
    }

    /// Return history lines after a command prefix.  The WebView's full
    /// history generator strips the already-typed prefix before presenting a
    /// row; the raw insert value is that same suffix, so accepting `git co`
    /// inserts `checkout ...`, never `git git ...`.
    pub(crate) fn history_suffix_suggestions(&self, prefix: &str, query: &str, fuzzy: bool) -> Vec<Suggestion> {
        let first_word_counts = self.next_word_counts(prefix.trim_end());
        let mut seen = HashSet::new();

        self.commands
            .iter()
            .rev()
            .filter_map(|(command, _)| {
                if !command.starts_with(prefix) {
                    return None;
                }
                let suffix = command[prefix.len()..].trim_end();
                if suffix.is_empty() || (!query.is_empty() && !crate::query::matches_query(suffix, query, fuzzy)) {
                    return None;
                }
                seen.insert(suffix.to_string()).then_some(suffix.to_string())
            })
            .take(HISTORY_LIMIT)
            .map(|suffix| {
                let first_word = suffix.split_whitespace().next().unwrap_or_default();
                let priority = first_word_counts
                    .get(first_word)
                    .copied()
                    .map_or(50, history_priority_base);
                Suggestion::new(suffix.clone(), "past command", "history")
                    .with_insert_value(suffix)
                    .with_priority(priority)
            })
            .collect()
    }

    fn next_word_counts(&self, prefix: &str) -> &HashMap<String, usize> {
        self.next_word_counts.get(prefix).unwrap_or(empty_word_counts())
    }
}

fn empty_word_counts() -> &'static HashMap<String, usize> {
    static EMPTY: OnceLock<HashMap<String, usize>> = OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

/// Same keys the per-request walk used: `""` plus every `command[..space]`.
fn next_word_counts(commands: &[(String, u64)]) -> HashMap<String, HashMap<String, usize>> {
    let mut counts = HashMap::new();
    for (command, _) in commands {
        record_next_words(command, &mut counts);
    }
    counts
}

fn record_next_words(command: &str, counts: &mut HashMap<String, HashMap<String, usize>>) {
    if let Some(word) = command.split_whitespace().next() {
        *counts
            .entry(String::new())
            .or_default()
            .entry(word.to_string())
            .or_default() += 1;
    }
    let mut search_from = 0;
    while let Some(rel) = command[search_from..].find(' ') {
        let space_at = search_from + rel;
        let prefix = command[..space_at].to_string();
        if let Some(word) = command[space_at + 1..].split_whitespace().next() {
            *counts.entry(prefix).or_default().entry(word.to_string()).or_default() += 1;
        }
        search_from = space_at + 1;
    }
}

/// Recent acceptance data used by the old WebView `updatePriorities` helper.
///
/// This intentionally stores the command and accepted spelling as separate
/// keys.  A `git add` acceptance must not promote an identically named `docker
/// add`, and aliases remain isolated by their primary spelling just as the
/// WebView's `makeArray(name)[0]` lookup was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AcceptanceIndex {
    entries: HashMap<String, HashMap<String, u64>>,
}

impl AcceptanceIndex {
    pub(crate) fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
    }

    pub fn load() -> Self {
        fig_settings::state::get_value(ACCEPTANCE_STATE_KEY)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    /// Return the last acceptance time in Unix milliseconds for a command and
    /// primary suggestion name.
    pub fn timestamp(&self, command: &str, name: &str) -> Option<u64> {
        self.entries.get(command).and_then(|items| items.get(name)).copied()
    }

    /// Update the in-memory index with an explicit timestamp.  This pure
    /// operation is useful to the engine and keeps tests independent of the
    /// user's settings database.
    pub fn record_at(&mut self, command: &str, name: &str, timestamp: u64) -> bool {
        let command = command.trim();
        let name = name.trim();
        if command.is_empty() || name.is_empty() || name == "↪" || name == "../" {
            return false;
        }
        self.entries
            .entry(command.to_string())
            .or_default()
            .insert(name.to_string(), timestamp);
        true
    }

    /// Record an acceptance and persist the complete map to SQLite-backed
    /// state. Persistence failures are deliberately best-effort: accepting a
    /// command must never fail the shell insertion or the completion worker.
    pub fn record(&mut self, command: &str, name: &str) -> bool {
        let timestamp = Self::now_millis();
        if !self.record_at(command, name, timestamp) {
            return false;
        }
        self.persist();
        true
    }

    pub fn persist(&self) {
        if let Ok(value) = serde_json::to_value(self) {
            let _ = fig_settings::state::set_value(ACCEPTANCE_STATE_KEY, value);
        }
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, String, u64)>) -> Self {
        let mut index = Self::default();
        for (command, name, timestamp) in entries {
            index.record_at(&command, &name, timestamp);
        }
        index
    }
}

fn first_word_count(counts: &HashMap<String, usize>, value: &str) -> usize {
    counts
        .get(value.split_whitespace().next().unwrap_or_default())
        .copied()
        .unwrap_or_default()
}

fn history_priority_base(count: usize) -> i64 {
    if count > 1 { 75 } else { 50 }
}

/// The old value is `75 + min(count, 10) / 10`. Compare in tenths so a
/// frequency of two remains 75.2 instead of being rounded up to 77.
fn history_priority_tenths(count: usize) -> i64 {
    match count {
        0 => 0,
        1 => 500,
        _ => 750 + count.min(10) as i64,
    }
}

fn bump(stats: &mut HashMap<String, (u32, u64)>, key: &str, ts: u64) {
    let entry = stats.entry(key.to_string()).or_insert((0, 0));
    entry.0 = entry.0.saturating_add(1);
    if ts > entry.1 {
        entry.1 = ts;
    }
}

#[allow(dead_code)]
pub fn apply(result: &mut CompleteResult, tokens: &[String], frecency: &Frecency) {
    apply_with_acceptance(result, tokens, frecency, &AcceptanceIndex::default(), "", false);
}

/// Rank a result using the same stable match/priority buckets as the legacy
/// WebView.  `alphabetical` disables acceptance recency, but deliberately does
/// not introduce a name tie-break: static lookup supplies category order and
/// stable sorting preserves it when priorities and match scores tie.
pub fn apply_with_acceptance(
    result: &mut CompleteResult,
    tokens: &[String],
    frecency: &Frecency,
    acceptance: &AcceptanceIndex,
    root_command: &str,
    alphabetical: bool,
) {
    let prefix = completion_prefix(tokens, matching_term(result));
    let query = matching_term(result).to_string();
    let history_counts = frecency.next_word_counts(&prefix);
    let root_command = if root_command.is_empty() {
        tokens.first().map(String::as_str).unwrap_or_default()
    } else {
        root_command
    };

    if query.is_empty() {
        result.suggestions.sort_by(|a, b| {
            compare_auto_execute(a, b)
                .then_with(|| compare_priority(b, a, history_counts, acceptance, root_command, !alphabetical))
        });
    } else {
        // `lookup` has already filtered rows with the request's fuzzy flag.
        // Running the scorer with fuzzy enabled here is safe for both modes:
        // non-fuzzy results contain no scattered matches, while fuzzy results
        // retain the old exact/prefix/fuzzy ordering.
        result.suggestions.sort_by(|a, b| {
            compare_auto_execute(a, b)
                .then_with(|| compare_match(a, b, &query))
                .then_with(|| compare_priority(b, a, history_counts, acceptance, root_command, !alphabetical))
        });
    }

    // This mirrors the old `deduplicateSuggestions` guard. It intentionally
    // stays off for large result sets so a generator cannot pay an O(n²) cost
    // or lose rows merely because the list is still being streamed.
    if result.suggestions.len() <= 50 {
        let mut deduplicated = Vec::with_capacity(result.suggestions.len());
        for suggestion in result.suggestions.drain(..) {
            if deduplicated.iter().any(|existing: &Suggestion| {
                existing.name == suggestion.name
                    && existing.insert_value == suggestion.insert_value
                    && existing.display_name == suggestion.display_name
                    && existing.args_hint == suggestion.args_hint
            }) {
                continue;
            }
            deduplicated.push(suggestion);
        }
        result.suggestions = deduplicated;
    }
}

fn compare_priority(
    left: &Suggestion,
    right: &Suggestion,
    history_counts: &HashMap<String, usize>,
    acceptance: &AcceptanceIndex,
    root_command: &str,
    use_acceptance: bool,
) -> Ordering {
    effective_priority_score(left, history_counts, acceptance, root_command, use_acceptance)
        .partial_cmp(&effective_priority_score(
            right,
            history_counts,
            acceptance,
            root_command,
            use_acceptance,
        ))
        .unwrap_or(Ordering::Equal)
}

fn compare_auto_execute(left: &Suggestion, right: &Suggestion) -> Ordering {
    let left_auto = left.kind == "auto-execute";
    let right_auto = right.kind == "auto-execute";
    right_auto.cmp(&left_auto)
}

fn effective_priority_tenths(suggestion: &Suggestion, history_counts: &HashMap<String, usize>) -> i64 {
    // Runtime constructors already apply `priority || 50` and clamp to
    // 0..100. Do not normalize zero again here: an explicit negative value
    // has already become a meaningful zero by this point.
    let ordinary = suggestion.priority.clamp(0, 100) * 10;
    let history = history_priority_tenths(first_word_count(history_counts, &suggestion.name));
    ordinary.max(history)
}

fn effective_priority_score(
    suggestion: &Suggestion,
    history_counts: &HashMap<String, usize>,
    acceptance: &AcceptanceIndex,
    root_command: &str,
    use_acceptance: bool,
) -> f64 {
    let mut priority = effective_priority_tenths(suggestion, history_counts) as f64 / 10.0;

    if use_acceptance && suggestion.kind != "auto-execute" && suggestion.name != "../" && suggestion.name != "↪" {
        let name = suggestion.primary_name.as_deref().unwrap_or(suggestion.name.as_str());
        if let Some(timestamp) = acceptance.timestamp(root_command, name) {
            // updatePriorities promotes priorities in [50, 75] to 75 before
            // adding the millisecond-derived fraction. A timestamp divided
            // by 1e13 is the exact scale used by the old JavaScript helper.
            if (50.0..=75.0).contains(&priority) {
                priority = 75.0;
            }
            priority += timestamp as f64 / 10_000_000_000_000.0;
        }
    }
    priority
}

fn compare_match(left: &Suggestion, right: &Suggestion, query: &str) -> Ordering {
    let left_score = best_match(left, query);
    let right_score = best_match(right, query);
    match (left_score, right_score) {
        (Some(left), Some(right)) => left
            .bucket()
            .cmp(&right.bucket())
            .then_with(|| right.score.cmp(&left.score)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn best_match(suggestion: &Suggestion, query: &str) -> Option<MatchScore> {
    let query = suggestion.query_term.as_deref().unwrap_or(query);
    let mut best = crate::query::match_score(&suggestion.name, query, true);
    if let Some(display_name) = suggestion.display_name.as_deref()
        && let Some(candidate) = crate::query::match_score(display_name, query, true)
        && best.is_none_or(|current| better_match(candidate, current))
    {
        best = Some(candidate);
    }
    best
}

fn better_match(left: MatchScore, right: MatchScore) -> bool {
    left.bucket() < right.bucket() || (left.bucket() == right.bucket() && left.score > right.score)
}

fn completion_prefix(tokens: &[String], search_term: &str) -> String {
    if tokens.is_empty() || (tokens.len() == 1 && !search_term.is_empty()) {
        return String::new();
    }
    if search_term.is_empty() {
        tokens.join(" ")
    } else {
        tokens[..tokens.len().saturating_sub(1)].join(" ")
    }
}

#[cfg(test)]
pub fn merge_history(result: &mut CompleteResult, tokens: &[String], frecency: &Frecency, fuzzy: bool) {
    merge_history_with_prefix(result, tokens, None, frecency, fuzzy);
}

pub fn merge_history_with_prefix(
    result: &mut CompleteResult,
    tokens: &[String],
    buffer_prefix: Option<String>,
    frecency: &Frecency,
    fuzzy: bool,
) {
    if tokens.is_empty() {
        return;
    }

    let query = matching_term(result);
    let history = if let Some(prefix) = buffer_prefix.or_else(|| history_prefix(tokens, query)) {
        frecency.history_suffix_suggestions(&prefix, query, fuzzy)
    } else {
        // While completing the first token, the old UI keeps the complete
        // command as both the display name and insertion value.
        frecency.history_suggestions(query, fuzzy, false)
    };

    // The WebView filters history against every spelling in a static
    // suggestion's `name` array. The IR keeps the selected spelling plus its
    // primary spelling; those are the alias identities that survive the
    // flattening step (displayName is presentation text, not an alias).
    let static_names: HashSet<String> = result
        .suggestions
        .iter()
        .flat_map(|suggestion| {
            suggestion
                .alias_names
                .iter()
                .map(String::as_str)
                .chain([suggestion.name.as_str()])
                .chain(suggestion.primary_name.as_deref())
                .map(str::to_string)
        })
        .collect();

    for item in history {
        if static_names.contains(&item.name) {
            continue;
        }
        result.suggestions.push(item);
    }
}

fn matching_term(result: &CompleteResult) -> &str {
    if result.match_term.is_empty() {
        result.search_term.as_str()
    } else {
        result.match_term.as_str()
    }
}

pub(crate) fn history_prefix(tokens: &[String], query: &str) -> Option<String> {
    if tokens.is_empty() || (tokens.len() == 1 && !query.is_empty()) {
        return None;
    }
    let prefix = if query.is_empty() {
        tokens.join(" ")
    } else {
        tokens[..tokens.len().saturating_sub(1)].join(" ")
    };
    if prefix.is_empty() {
        None
    } else {
        Some(format!("{prefix} "))
    }
}

/// WebView `getFullHistorySuggestions` slices the original command buffer:
/// `originalNode.startIndex + text.length - innerText.length`. Quotes and
/// doubled spaces stay in the prefix; alias expansion does not.
pub(crate) fn history_prefix_from_buffer(
    command_buffer: &str,
    ends_with_space: bool,
    tokens: &[String],
) -> Option<String> {
    if tokens.is_empty() || (tokens.len() == 1 && !ends_with_space) {
        return None;
    }
    if ends_with_space {
        let mut prefix = command_buffer.to_string();
        if !prefix.ends_with(' ') {
            prefix.push(' ');
        }
        return Some(prefix);
    }
    let raw = crate::lookup::current_token_raw(command_buffer);
    let inner = tokens.last().map(String::as_str).unwrap_or_default();
    if command_buffer.len() < raw.len() {
        return history_prefix(tokens, inner);
    }
    let start = command_buffer.len() - raw.len();
    // Quotes are part of the original node text and stay in the prefix.
    // Backslash-escaped spaces are not: copying `text.length - innerText`
    // would swallow the first character of the token and miss history rows.
    let prefix_end = if raw.starts_with(['\'', '"']) || raw.starts_with("$'") {
        start.saturating_add(raw.len().saturating_sub(inner.len()))
    } else {
        start
    };
    let prefix = command_buffer.get(..prefix_end.min(command_buffer.len()))?;
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    }
}

/// Load the complete local history database.  This remains public for the
/// headless/diagnostic callers; the engine uses [`load_commands_for`] so the
/// selected source follows the legacy custom-command and shell settings.
#[allow(dead_code)]
pub fn load_commands() -> Vec<(String, u64)> {
    load_commands_for(&HistorySourceConfig {
        custom_command: None,
        all_shells: true,
        current_shell: HistoryShell::Unknown,
    })
}

/// Load history for the source selected by the current request and settings.
///
/// A configured custom command is run through the same default zsh path as
/// the old WebView implementation. Its output wins when it is non-empty; a
/// failure, timeout, or empty output falls back to the complete local history
/// database. Without a custom command, known shells are filtered to the
/// current shell (or zsh+bash when `allShells` is enabled); unknown shells use
/// the complete database source.
pub(crate) fn load_commands_for(config: &HistorySourceConfig) -> Vec<(String, u64)> {
    if let Some(command) = config.custom_command.as_deref() {
        if let Some(commands) = run_custom_history(command) {
            return history_commands_from_output(&commands);
        }
        return load_database_commands(|_| true);
    }

    let mut from_shell = Vec::new();
    match config.current_shell {
        HistoryShell::Zsh => {
            from_shell.extend(login_history_commands("zsh", "fc -R; fc -ln 1"));
        },
        HistoryShell::Bash => {
            from_shell.extend(login_history_commands("bash", "fc -ln 1"));
        },
        HistoryShell::Fish => {
            from_shell.extend(login_history_commands("fish", "history search"));
        },
        HistoryShell::Unknown => {},
    }
    if config.all_shells {
        if config.current_shell != HistoryShell::Zsh {
            from_shell.extend(login_history_commands("zsh", "fc -R; fc -ln 1"));
        }
        if config.current_shell != HistoryShell::Bash {
            from_shell.extend(login_history_commands("bash", "fc -ln 1"));
        }
    }
    if from_shell.is_empty() {
        match config.current_shell {
            HistoryShell::Unknown => load_database_commands(|_| true),
            _ => load_database_commands(|row_shell| includes_database_shell(config, *row_shell)),
        }
    } else {
        from_shell
    }
}

fn login_history_commands(shell: &str, command: &str) -> Vec<(String, u64)> {
    crate::process::try_execute_isolated_success(shell, &["-lc".into(), command.into()], "", CUSTOM_HISTORY_TIMEOUT)
        .map(|output| history_commands_from_output(&output))
        .unwrap_or_default()
}

fn includes_database_shell(config: &HistorySourceConfig, row_shell: HistoryShell) -> bool {
    match config.current_shell {
        HistoryShell::Unknown => true,
        HistoryShell::Zsh | HistoryShell::Bash if config.all_shells => {
            matches!(row_shell, HistoryShell::Zsh | HistoryShell::Bash)
        },
        current => row_shell == current,
    }
}

fn non_empty_history_command(command: &str) -> Option<String> {
    (!command.trim().is_empty()).then(|| command.to_string())
}

fn history_commands_from_output(output: &str) -> Vec<(String, u64)> {
    output
        .lines()
        .filter_map(non_empty_history_command)
        .map(|command| (command, 0))
        .collect()
}

/// Cap on how much shell history feeds frecency and history suggestions.
///
/// The engine reloads this after every watchdog reset, and ranking walks the
/// loaded list per request, so an unbounded `all_rows` scan made both scale
/// with the lifetime size of the history database. Recent commands are the
/// only ones frecency meaningfully weights anyway.
const MAX_DATABASE_HISTORY_COMMANDS: usize = 10_000;

fn load_database_commands<F>(include: F) -> Vec<(String, u64)>
where
    F: Fn(&HistoryShell) -> bool,
{
    use fig_settings::history::{HistoryColumn, Order, OrderBy};
    let history = fig_settings::history::History::new();
    // Newest first via the integer primary key (start_time is unindexed),
    // then restored to chronological order, which history_suggestions
    // depends on to surface the most recent match.
    let mut rows = history
        .rows(
            None,
            vec![OrderBy::new(HistoryColumn::Id, Order::Desc)],
            MAX_DATABASE_HISTORY_COMMANDS,
            0,
        )
        .unwrap_or_default();
    rows.reverse();
    rows.into_iter()
        .filter_map(|row| {
            let shell = normalize_history_shell(row.shell.as_deref());
            if !include(&shell) {
                return None;
            }
            let command = non_empty_history_command(row.command.as_deref()?)?;
            let ts = row
                .start_time
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            Some((command, ts))
        })
        .collect()
}

/// Execute the configured history command without tying up the desktop event
/// loop. This function runs inside the engine attempt worker, and also has its
/// own legacy five-second deadline so a shell command cannot consume the
/// worker watchdog indefinitely.
fn run_custom_history(command: &str) -> Option<String> {
    // Killing only `zsh` can leave a background descendant holding stdout
    // open forever. The shared runner isolates and kills the whole process
    // group, drains output without blocking, and always reaps the shell.
    let output = crate::process::try_execute_isolated_success(
        "zsh",
        &["-c".into(), command.into()],
        "",
        CUSTOM_HISTORY_TIMEOUT,
    )?;
    (!output.trim().is_empty()).then_some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_match_priority_keeps_input_order_without_shell_frecency_tie_break() {
        let frecency = Frecency::from_commands([("checkout".into(), 900), ("cherry-pick".into(), 1)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("cherry-pick", "Apply commits", "subcommand"),
                Suggestion::new("checkout", "Switch branches", "subcommand"),
            ],
            search_term: "ch".into(),
            match_term: "ch".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["ch".into()], &frecency);
        assert_eq!(result.suggestions[0].name, "cherry-pick");
        assert_eq!(result.suggestions[1].name, "checkout");
    }

    #[test]
    fn exact_and_case_insensitive_matches_beat_prefixes() {
        let frecency = Frecency::default();
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("git-status", "", "subcommand"),
                Suggestion::new("Git", "", "subcommand"),
                Suggestion::new("git", "", "subcommand"),
            ],
            search_term: "git".into(),
            match_term: "git".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["git".into()], &frecency);
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["git", "Git", "git-status"]
        );
    }

    #[test]
    fn ranking_uses_each_suggestions_query_term_override() {
        let frecency = Frecency::default();
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("scope@foobar", "", "arg"),
                Suggestion::new("foo", "", "arg").with_query_term(Some("foo".into())),
            ],
            search_term: "scope@foo".into(),
            match_term: "scope@foo".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["cmd".into(), "scope@foo".into()], &frecency);
        assert_eq!(result.suggestions[0].name, "foo");
        assert_eq!(result.suggestions[1].name, "scope@foobar");
    }

    #[test]
    fn empty_query_keeps_equal_priority_input_order() {
        let frecency = Frecency::from_commands([("git alpha".into(), 10_000), ("git zeta".into(), 1)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("zeta", "", "subcommand"),
                Suggestion::new("alpha", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["git".into()], &frecency);
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    #[test]
    fn auto_execute_precedes_clamped_normal_priorities() {
        let frecency = Frecency::default();
        let low = Suggestion::new("low", "", "arg").with_priority(-4);
        let zero = Suggestion::new("zero", "", "arg").with_priority(0);
        let high = Suggestion::new("high", "", "arg").with_priority(900);
        let mut auto = Suggestion::new("↪", "", "auto-execute");
        auto.priority = 0;
        let mut result = CompleteResult {
            suggestions: vec![low, zero, high, auto],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &[], &frecency);
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["↪", "high", "zero", "low"]
        );
        assert_eq!(result.suggestions[1].priority, 100);
        assert_eq!(result.suggestions[2].priority, 50);
        assert_eq!(result.suggestions[3].priority, 0);
    }

    #[test]
    fn acceptance_recency_promotes_only_the_matching_root_and_name() {
        let acceptance = AcceptanceIndex::from_entries([
            ("git".into(), "push".into(), 2_000_000_000_000),
            ("docker".into(), "run".into(), 2_000_000_000_000),
        ]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "", "subcommand"),
                Suggestion::new("push", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut result,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(result.suggestions[0].name, "push");

        let mut isolated = CompleteResult {
            suggestions: vec![
                Suggestion::new("run", "", "subcommand"),
                Suggestion::new("status", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut isolated,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(
            isolated.suggestions[0].name, "run",
            "docker's acceptance must not leak into git"
        );
    }

    #[test]
    fn acceptance_update_is_visible_to_the_next_ranking_request() {
        let mut acceptance = AcceptanceIndex::default();
        let mut before = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "", "subcommand"),
                Suggestion::new("push", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut before,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(before.suggestions[0].name, "status");

        acceptance.record_at("git", "push", 2_000_000_000_000);
        let mut after = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "", "subcommand"),
                Suggestion::new("push", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut after,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            false,
        );
        assert_eq!(after.suggestions[0].name, "push");
    }

    #[test]
    fn acceptance_index_round_trips_and_excludes_navigation_rows() {
        let mut index = AcceptanceIndex::default();
        assert!(!index.record_at("git", "↪", 10));
        assert!(!index.record_at("git", "../", 10));
        assert!(index.record_at("git", "status", 123));
        let value = serde_json::to_value(&index).expect("serialize acceptance index");
        let restored: AcceptanceIndex = serde_json::from_value(value).expect("deserialize acceptance index");
        assert_eq!(restored.timestamp("git", "status"), Some(123));
        assert_eq!(restored.timestamp("git", "↪"), None);
    }

    #[test]
    fn alphabetical_ranking_disables_acceptance_but_deduplicates_stably() {
        let acceptance = AcceptanceIndex::from_entries([("git".into(), "alpha".into(), 2_000_000_000_000)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("zeta", "", "subcommand"),
                Suggestion::new("alpha", "", "subcommand"),
                Suggestion::new("alpha", "", "subcommand"),
            ],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply_with_acceptance(
            &mut result,
            &["git".into()],
            &Frecency::default(),
            &acceptance,
            "git",
            true,
        );
        assert_eq!(
            result
                .suggestions
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["zeta", "alpha"]
        );
    }

    #[test]
    fn first_token_inserts_matching_history() {
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40)]);
        let mut result = CompleteResult {
            suggestions: Vec::new(),
            search_term: "git".into(),
            match_term: "git".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into()], &frecency, false);
        assert!(
            result
                .suggestions
                .iter()
                .any(|s| s.name.starts_with("git checkout") && s.kind == "history"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn history_uses_recent_source_order_and_last_duplicate_wins() {
        // History rows arrive oldest-first (including custom-command output),
        // so the old generator walks them backwards before deduplicating.
        // Timestamps are intentionally misleading: they must not reorder the
        // source sequence by shell frecency.
        let frecency = Frecency::from_commands([("git old".into(), 900), ("git new".into(), 1), ("git old".into(), 2)]);
        assert_eq!(
            frecency
                .history_suffix_suggestions("git ", "", false)
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["old", "new"]
        );
        assert_eq!(
            frecency
                .history_suggestions("git", false, false)
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["git old", "git new"]
        );
    }

    #[test]
    fn history_dedup_considers_flattened_static_aliases() {
        let frecency = Frecency::from_commands([("git checkout".into(), 1)]);
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("co", "", "subcommand")
                    .with_primary_name(Some("checkout".into()))
                    .with_alias_names(vec!["checkout".into(), "co".into()]),
            ],
            search_term: "co".into(),
            match_term: "co".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into(), "co".into()], &frecency, false);
        assert!(result.suggestions.iter().all(|item| item.kind != "history"));
    }

    #[test]
    fn history_after_partial_second_token_uses_only_the_suffix() {
        let frecency = Frecency::from_commands([
            ("git commit -m feature".into(), 40),
            ("git cherry-pick main".into(), 30),
        ]);
        assert!(crate::query::matches_query("commit -m feature", "co", false));
        assert_eq!(
            frecency
                .history_suffix_suggestions("git ", "co", false)
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["commit -m feature"]
        );
        let mut result = CompleteResult {
            suggestions: vec![Suggestion::new("commit", "Record changes", "subcommand")],
            search_term: "'co".into(),
            match_term: "co".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into(), "co".into()], &frecency, false);
        let history = result
            .suggestions
            .iter()
            .find(|item| item.kind == "history")
            .unwrap_or_else(|| panic!("history suffix missing from {:?}", result.suggestions));
        assert_eq!(history.name, "commit -m feature");
        assert_eq!(history.insert_value.as_deref(), Some("commit -m feature"));
        assert!(!history.name.starts_with("git "));
    }

    #[test]
    fn history_frequency_boosts_the_matching_static_first_word() {
        let frecency =
            Frecency::from_commands([("git checkout -b one".into(), 10), ("git checkout -b two".into(), 20)]);
        let mut explicit_76 = Suggestion::new("cherry-pick", "", "subcommand");
        explicit_76.priority = 76;
        let mut result = CompleteResult {
            suggestions: vec![explicit_76, Suggestion::new("checkout", "", "subcommand")],
            search_term: "ch".into(),
            match_term: "ch".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into(), "ch".into()], &frecency, false);
        apply(&mut result, &["git".into(), "ch".into()], &frecency);

        // Two history hits are 75.2, so they boost static checkout above 75
        // but do not leap over an explicit priority of 76.
        assert_eq!(result.suggestions[0].name, "cherry-pick");
        let checkout = result
            .suggestions
            .iter()
            .position(|item| item.name == "checkout")
            .expect("static checkout");
        let checkout_history = result
            .suggestions
            .iter()
            .position(|item| item.kind == "history")
            .expect("checkout history");
        assert!(checkout < checkout_history);
    }

    #[test]
    fn duplicate_history_entries_count_before_dedup() {
        let frecency = Frecency::from_commands([
            ("git checkout -b feature".into(), 10),
            ("git checkout -b feature".into(), 20),
        ]);
        let history = frecency.history_suffix_suggestions("git ", "ch", false);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].priority, 75);
    }

    #[test]
    fn trailing_space_inserts_history_suffixes_without_repeating_the_command() {
        let frecency = Frecency::from_commands([("git checkout -b feature".into(), 40), ("npm test".into(), 50)]);
        let mut result = CompleteResult {
            suggestions: vec![Suggestion::new("checkout", "", "subcommand")],
            search_term: String::new(),
            match_term: String::new(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        merge_history(&mut result, &["git".into()], &frecency, false);
        assert!(
            result.suggestions.iter().all(|s| !s.name.starts_with("git ")),
            "{:?}",
            result.suggestions
        );
        assert!(
            result
                .suggestions
                .iter()
                .any(|s| s.name == "checkout -b feature" && s.kind == "history"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn dedup_matches_old_fields_and_ignores_description_and_kind() {
        let frecency = Frecency::default();
        let mut result = CompleteResult {
            suggestions: vec![
                Suggestion::new("status", "one", "subcommand"),
                Suggestion::new("status", "two", "subcommand"),
                Suggestion::new("status", "", "history").with_insert_value("status"),
                Suggestion::new("status", "", "history").with_insert_value("status"),
            ],
            search_term: "st".into(),
            match_term: "st".into(),
            fuzzy: false,
            ..CompleteResult::default()
        };
        apply(&mut result, &["git".into(), "st".into()], &frecency);
        assert_eq!(result.suggestions.len(), 2);
        assert_eq!(result.suggestions[0].kind, "subcommand");
        assert_eq!(result.suggestions[1].kind, "history");
    }

    #[test]
    fn history_shell_normalization_accepts_paths_and_figterm_suffixes() {
        assert_eq!(normalize_history_shell(Some("/bin/zsh")), HistoryShell::Zsh);
        assert_eq!(normalize_history_shell(Some("bash (figterm)")), HistoryShell::Bash);
        assert_eq!(normalize_history_shell(Some("/usr/local/bin/fish")), HistoryShell::Fish);
        assert_eq!(normalize_history_shell(Some("nu")), HistoryShell::Unknown);
        assert_eq!(normalize_history_shell(None), HistoryShell::Unknown);
    }

    #[test]
    fn history_source_key_changes_only_with_source_inputs() {
        let zsh = history_source_config(None, false, Some("/bin/zsh"), None);
        let zsh_again = history_source_config(None, false, Some("zsh"), Some("bash"));
        let zsh_from_process = history_source_config(None, false, Some("unknown"), Some("/bin/zsh"));
        let bash = history_source_config(None, false, Some("bash"), None);
        let all_shells = history_source_config(None, true, Some("zsh"), None);
        let custom = history_source_config(Some("fc -ln 1".into()), false, Some("zsh"), None);

        assert_eq!(zsh, zsh_again);
        assert_eq!(zsh, zsh_from_process);
        assert_ne!(zsh, bash);
        assert_ne!(zsh, all_shells);
        assert_ne!(zsh, custom);
    }

    #[test]
    fn custom_history_output_keeps_lines_and_discards_blank_lines() {
        assert_eq!(
            history_commands_from_output("git status\n\n git add . \n"),
            vec![("git status".into(), 0), (" git add . ".into(), 0)]
        );
        assert!(history_commands_from_output("\n  \n").is_empty());
    }

    #[test]
    fn history_shell_selection_matches_legacy_sources() {
        let zsh = history_source_config(None, false, Some("zsh"), None);
        assert!(includes_database_shell(&zsh, HistoryShell::Zsh));
        assert!(!includes_database_shell(&zsh, HistoryShell::Bash));
        assert!(!includes_database_shell(&zsh, HistoryShell::Fish));

        let merged = history_source_config(None, true, Some("/bin/bash"), None);
        assert!(includes_database_shell(&merged, HistoryShell::Zsh));
        assert!(includes_database_shell(&merged, HistoryShell::Bash));
        assert!(!includes_database_shell(&merged, HistoryShell::Fish));

        let fish = history_source_config(None, true, Some("fish"), None);
        assert!(includes_database_shell(&fish, HistoryShell::Fish));
        assert!(!includes_database_shell(&fish, HistoryShell::Zsh));

        let unknown = history_source_config(None, false, Some("nu"), None);
        assert!(includes_database_shell(&unknown, HistoryShell::Zsh));
        assert!(includes_database_shell(&unknown, HistoryShell::Bash));
        assert!(includes_database_shell(&unknown, HistoryShell::Fish));
    }

    fn walk_next_word_counts(commands: &[(String, u64)], prefix: &str) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        let full_prefix = (!prefix.is_empty()).then(|| format!("{prefix} "));
        for (command, _) in commands {
            let suffix = match full_prefix.as_deref() {
                Some(prefix) => match command.strip_prefix(prefix) {
                    Some(suffix) => suffix,
                    None => continue,
                },
                None => command.as_str(),
            };
            let first_word = suffix.split_whitespace().next().unwrap_or_default();
            if !first_word.is_empty() {
                *counts.entry(first_word.to_string()).or_default() += 1;
            }
        }
        counts
    }

    #[test]
    fn next_word_index_matches_the_per_request_walk() {
        let commands = [
            ("git checkout main".into(), 1_u64),
            ("git status".into(), 2),
            ("echo hi".into(), 3),
            ("git  checkout".into(), 4),
        ];
        let frecency = Frecency::from_commands(commands.clone());
        for prefix in ["", "git", "git checkout", "echo", "missing", "git "] {
            assert_eq!(
                frecency.next_word_counts(prefix),
                &walk_next_word_counts(&commands, prefix),
                "prefix {prefix:?}"
            );
        }
    }

    #[test]
    fn history_prefix_keeps_quotes_and_double_spaces_from_the_buffer() {
        let (tokens, ends) = crate::lookup::tokenize("git  'ch");
        assert_eq!(
            history_prefix_from_buffer("git  'ch", ends, &tokens).as_deref(),
            Some("git  '")
        );
        let (tokens, ends) = crate::lookup::tokenize("g checkout");
        assert_eq!(
            history_prefix_from_buffer("g checkout", ends, &tokens).as_deref(),
            Some("g ")
        );
        let (tokens, ends) = crate::lookup::tokenize("git ");
        assert_eq!(
            history_prefix_from_buffer("git ", ends, &tokens).as_deref(),
            Some("git ")
        );
        let (tokens, ends) = crate::lookup::tokenize("git");
        assert_eq!(history_prefix_from_buffer("git", ends, &tokens), None);
    }
}
