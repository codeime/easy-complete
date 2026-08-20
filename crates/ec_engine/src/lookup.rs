//! IRIS-style token walk over static spec IR.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::ir::{ArgSpec, OptionSpec, ParserDirectives, Registry, Spec, SuggestionMeta};
use crate::query::matches_query;
use crate::runtime::{CompleteRequest, CompleteResult, CurrentArg, Suggestion, query_term_for, suggestion_query_term};

pub fn tokenize(buffer: &str) -> (Vec<String>, bool) {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut started = false;
    let mut quote = None;
    let mut escaped = false;
    let mut trailing_space = false;

    for ch in buffer.chars() {
        if escaped {
            token.push(ch);
            started = true;
            escaped = false;
            trailing_space = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            trailing_space = false;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else {
                token.push(ch);
            }
            started = true;
            trailing_space = false;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                started = true;
                trailing_space = false;
            },
            ch if ch.is_whitespace() => {
                if started {
                    tokens.push(std::mem::take(&mut token));
                    started = false;
                }
                trailing_space = true;
            },
            ch => {
                token.push(ch);
                started = true;
                trailing_space = false;
            },
        }
    }
    if escaped {
        // Preserve a dangling escape in the logical token.  The raw query
        // remains available through current_token_raw for insertion.
        token.push('\\');
        started = true;
        trailing_space = false;
    }
    if started {
        tokens.push(token);
    }
    (tokens, trailing_space && quote.is_none())
}

/// Return the raw shell token under the caret.  Matching uses the normalized
/// token returned by [`tokenize`], while insertion needs the exact bytes that
/// must be deleted (including quotes and escaped spaces).
pub fn current_token_raw(buffer: &str) -> String {
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in buffer.char_indices() {
        if escaped {
            escaped = false;
            if start.is_none() {
                start = Some(index);
            }
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            if start.is_none() {
                start = Some(index);
            }
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            if start.is_none() {
                start = Some(index);
            }
        } else if ch.is_whitespace() {
            start = None;
        } else if start.is_none() {
            start = Some(index);
        }
    }
    start.map_or_else(String::new, |index| buffer[index..].to_string())
}

/// Return the innermost command text under the caret after shell separators.
///
/// `complete` and `ranking_root_command` must share this slice so
/// `echo x && git ch` ranks and completes as `git`, not `echo`.
pub fn current_command_slice(buffer: &str) -> &str {
    let start = innermost_command_start(buffer);
    skip_leading_assignments(buffer.get(start..).unwrap_or_default())
}

/// Buffer used by lookup, ranking, and history: caret slice, then the current
/// command after separators and leading assignments.
pub fn completion_buffer(buffer: &str, cursor: Option<u32>) -> &str {
    current_command_slice(buffer_before_cursor(buffer, cursor))
}

/// True when the caret is in a new command that has no tokens yet
/// (`echo x && `), as opposed to an empty prompt or an assignment-only line.
pub fn is_fresh_command_position(buffer: &str) -> bool {
    let start = innermost_command_start(buffer);
    start > 0
        && buffer.ends_with(|ch: char| ch.is_whitespace())
        && buffer
            .get(start..)
            .is_some_and(|rest| rest.chars().all(char::is_whitespace))
}

fn peek_char(buffer: &str, index: usize) -> Option<char> {
    buffer.get(index..)?.chars().next()
}

fn skip_ws_bytes(buffer: &str, mut index: usize) -> usize {
    while index < buffer.len() {
        let Some(ch) = peek_char(buffer, index) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

/// Shell list/pipeline operators, longest match first. Redirections such as
/// `2>&1` and `&>` are not command separators.
fn operator_len(buffer: &str, index: usize) -> Option<usize> {
    let rest = buffer.get(index..)?;
    let mut chars = rest.chars();
    let ch = chars.next()?;
    let next = chars.next();
    match (ch, next) {
        ('&' | '|', Some('&')) | ('&', Some(';')) | ('|', Some('|')) => Some(2),
        (';' | '|', _) => Some(1),
        ('&', _) => {
            let prev = buffer.get(..index).and_then(|prefix| prefix.chars().next_back());
            if matches!(prev, Some('>' | '<')) || matches!(next, Some('>')) {
                None
            } else {
                Some(1)
            }
        },
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Root,
    Subshell,
    Compound,
    Backtick,
}

struct CommandFrame {
    start: usize,
    kind: FrameKind,
}

fn at_statement_boundary(buffer: &str, command_start: usize, current: usize) -> bool {
    buffer
        .get(command_start..current)
        .is_some_and(|prefix| skip_leading_assignments(prefix).chars().all(char::is_whitespace))
}

fn pop_frame(frames: &mut Vec<CommandFrame>, kind: FrameKind) -> bool {
    if frames.last().is_some_and(|frame| frame.kind == kind) {
        frames.pop();
        true
    } else {
        false
    }
}

fn innermost_command_start(buffer: &str) -> usize {
    let mut frames = vec![CommandFrame {
        start: 0,
        kind: FrameKind::Root,
    }];
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < buffer.len() {
        let Some(ch) = peek_char(buffer, index) else {
            break;
        };
        let ch_len = ch.len_utf8();
        if escaped {
            escaped = false;
            index += ch_len;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            index += ch_len;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
            index += ch_len;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            index += ch_len;
            continue;
        }
        if ch == '$' {
            if peek_char(buffer, index + ch_len) == Some('(') {
                frames.push(CommandFrame {
                    start: index + ch_len + 1,
                    kind: FrameKind::Subshell,
                });
                index += ch_len + 1;
                continue;
            }
            index += ch_len;
            continue;
        }
        if (ch == '<' || ch == '>') && peek_char(buffer, index + ch_len) == Some('(') {
            frames.push(CommandFrame {
                start: index + ch_len + 1,
                kind: FrameKind::Subshell,
            });
            index += ch_len + 1;
            continue;
        }
        if ch == '`' {
            if !pop_frame(&mut frames, FrameKind::Backtick) {
                frames.push(CommandFrame {
                    start: index + ch_len,
                    kind: FrameKind::Backtick,
                });
            }
            index += ch_len;
            continue;
        }
        let command_start = frames.last().map_or(0, |frame| frame.start);
        if ch == '(' && at_statement_boundary(buffer, command_start, index) {
            frames.push(CommandFrame {
                start: index + ch_len,
                kind: FrameKind::Subshell,
            });
            index += ch_len;
            continue;
        }
        if ch == '{' && at_statement_boundary(buffer, command_start, index) {
            frames.push(CommandFrame {
                start: index + ch_len,
                kind: FrameKind::Compound,
            });
            index += ch_len;
            continue;
        }
        if ch == ')' && pop_frame(&mut frames, FrameKind::Subshell) {
            index += ch_len;
            continue;
        }
        if ch == '}' && pop_frame(&mut frames, FrameKind::Compound) {
            index += ch_len;
            continue;
        }
        if let Some(op_len) = operator_len(buffer, index) {
            let next_start = skip_ws_bytes(buffer, index + op_len);
            if let Some(frame) = frames.last_mut() {
                frame.start = next_start;
            }
            index += op_len;
            continue;
        }
        index += ch_len;
    }
    frames.last().map_or(0, |frame| frame.start)
}

fn is_fd_redirection_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    let mut index = 0;
    let mut saw_fd = false;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        saw_fd = true;
        index += 1;
    }
    match bytes.get(index) {
        Some(b'>' | b'<') => {
            index += 1;
            let mut saw_amp = false;
            if bytes.get(index) == Some(&b'&') {
                saw_amp = true;
                index += 1;
            }
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
            index == bytes.len() && (saw_fd || saw_amp)
        },
        _ => false,
    }
}

fn is_assignment_token(token: &str) -> bool {
    let mut chars = token.chars();
    let mut saw_name = false;
    for ch in chars.by_ref() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '[' || ch == ']' {
            saw_name = true;
            continue;
        }
        if ch == '+' {
            return saw_name && chars.next() == Some('=');
        }
        return saw_name && ch == '=';
    }
    false
}

fn raw_first_token_end(buffer: &str) -> usize {
    let mut started = false;
    let mut quote = None;
    let mut escaped = false;
    let mut end = 0;
    for (index, ch) in buffer.char_indices() {
        if escaped {
            escaped = false;
            started = true;
            end = index + ch.len_utf8();
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            end = index + ch.len_utf8();
            continue;
        }
        if let Some(active) = quote {
            started = true;
            end = index + ch.len_utf8();
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
            end = index + ch.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            if started {
                return index;
            }
            continue;
        }
        started = true;
        end = index + ch.len_utf8();
    }
    if started { end } else { 0 }
}

fn skip_leading_assignments(slice: &str) -> &str {
    let mut rest = slice;
    loop {
        let start = skip_ws_bytes(rest, 0);
        if start >= rest.len() {
            return "";
        }
        let candidate = rest.get(start..).unwrap_or_default();
        let Some(first) = tokenize(candidate).0.into_iter().next() else {
            return "";
        };
        let token_end = raw_first_token_end(candidate);
        if token_end == 0 {
            return candidate;
        }
        let raw_token = candidate.get(..token_end).unwrap_or_default();
        // Quoted words are never assignments; the old parser matches `name=`
        // against the raw buffer, so `'FOO=1' git` keeps `FOO=1` as argv0.
        if raw_token.starts_with(['\'', '"']) || !(is_assignment_token(&first) || is_fd_redirection_token(&first)) {
            return candidate;
        }
        rest = candidate.get(token_end..).unwrap_or_default();
    }
}

/// Commands listed by `autocomplete.disableForCommands` are intentionally a
/// hard opt-out: do not load their spec-backed rows or invoke any fallback
/// generator.  The setting is an array in the WebView configuration, so a
/// malformed value is treated as unset rather than disabling completion for
/// every command.
fn command_is_disabled_from(commands: &[String], command: &str) -> bool {
    commands.iter().any(|disabled| disabled == command)
}

pub(crate) fn command_is_disabled(command: &str) -> bool {
    fig_settings::settings::get::<Vec<String>>("autocomplete.disableForCommands")
        .ok()
        .flatten()
        .is_some_and(|commands| command_is_disabled_from(&commands, command))
}

#[derive(Debug)]
pub(crate) struct ActiveArg {
    pub arg: ArgSpec,
    /// Normalized value used to filter generator output.
    pub query: String,
    /// Raw shell text used as the result search term for deletion.
    pub search_term: String,
    /// Mandatory option arguments and explicit `--option=value` forms are
    /// exclusive completion contexts: sibling options and subcommands must
    /// not replace their argument suggestions. A separated optional value is
    /// intentionally non-exclusive because it may also be the next option.
    pub exclusive: bool,
}

#[derive(Debug)]
pub(crate) struct CompletionContext {
    pub spec: Arc<Spec>,
    pub active_arg: Option<ActiveArg>,
    /// The effective persistent option set after walking through any parent
    /// subcommands/loadSpecs. The WebView parser mutates this set onto the
    /// child completion object before collecting rows.
    pub persistent_options: Vec<OptionSpec>,
    /// Options consumed in the current parser scope. Entering a subcommand or
    /// a loadSpec resets this list, matching `passedOptions` in the WebView.
    pub passed_options: Vec<OptionSpec>,
    /// Parser directives inherited along the current spec path.
    pub parser_directives: ParserDirectives,
    /// Whether sibling options are legal at the current parser state.
    pub options_allowed: bool,
}

#[derive(Debug, Clone)]
struct OptionArgState {
    arg: ArgSpec,
    /// Number of values already consumed for this option argument. A zero
    /// count means the flag was entered and is waiting for its first value.
    count: usize,
}

fn merge_parser_directives(parent: &ParserDirectives, spec: &Spec) -> ParserDirectives {
    let child = spec.parser_directives.as_ref();
    ParserDirectives {
        options_must_precede_arguments: child
            .and_then(|directives| directives.options_must_precede_arguments)
            .or(parent.options_must_precede_arguments),
        flags_are_posix_noncompliant: child
            .and_then(|directives| directives.flags_are_posix_noncompliant)
            .or(parent.flags_are_posix_noncompliant),
        option_arg_separators: child
            .and_then(|directives| directives.option_arg_separators.clone())
            .or_else(|| parent.option_arg_separators.clone()),
    }
}

fn inherit_subcommand_directives(parent: &ParserDirectives, spec: &Spec) -> ParserDirectives {
    spec.parser_directives.clone().unwrap_or_else(|| parent.clone())
}

fn options_can_break_variadic_arg(arg: &ArgSpec) -> bool {
    arg.options_can_break_variadic_arg != Some(false)
}

fn can_consume_subcommands(option_arg: Option<&OptionArgState>, entered_args: bool) -> bool {
    !option_arg.is_some_and(|state| state.arg.is_variadic || !state.arg.is_optional) && !entered_args
}

fn can_consume_options(
    directives: &ParserDirectives,
    after_double_dash: bool,
    entered_args: bool,
    option_arg: Option<&OptionArgState>,
    subcommand_arg: Option<&ArgSpec>,
    subcommand_variadic_count: usize,
) -> bool {
    if after_double_dash {
        return false;
    }
    if directives.options_must_precede_arguments == Some(true) && entered_args {
        return false;
    }
    if let Some(state) = option_arg
        && (state.arg.is_variadic || !state.arg.is_optional)
    {
        return state.arg.is_variadic && state.count > 0 && options_can_break_variadic_arg(&state.arg);
    }
    if let Some(arg) = subcommand_arg
        && arg.is_variadic
        && subcommand_variadic_count > 0
        && !options_can_break_variadic_arg(arg)
    {
        return false;
    }
    true
}

#[derive(Debug, Clone, Copy)]
struct ShortOptionChain<'spec, 'token> {
    option: &'spec OptionSpec,
    /// Portion of the token occupied by short options, excluding an attached
    /// value (`-amfoo` -> `-am`).
    prefix: &'token str,
    attached_value: Option<&'token str>,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedOptionToken<'spec, 'token> {
    option: &'spec OptionSpec,
    attached_value: Option<&'token str>,
    attached_separator: Option<char>,
    chain: Option<ShortOptionChain<'spec, 'token>>,
}

pub(crate) fn resolve_context(
    root: Arc<Spec>,
    tokens: &[String],
    ends_with_space: bool,
    query: &str,
    raw_query: &str,
    registry: Option<&mut Registry>,
) -> CompletionContext {
    let current_limit = if ends_with_space {
        tokens.len()
    } else {
        tokens.len().saturating_sub(1)
    };
    let (spec, path_end, persistent_options, passed_options, parser_directives, options_allowed) =
        walk_spec(root, tokens, current_limit, registry);
    let persistent_refs: Vec<&OptionSpec> = persistent_options.iter().collect();
    let active = active_arg(
        spec.as_ref(),
        &persistent_refs,
        tokens,
        path_end,
        current_limit,
        ends_with_space,
        query,
        raw_query,
        &parser_directives,
    );
    CompletionContext {
        spec,
        active_arg: active,
        persistent_options,
        passed_options,
        parser_directives,
        options_allowed,
    }
}

fn walk_spec(
    root: Arc<Spec>,
    tokens: &[String],
    limit: usize,
    mut registry: Option<&mut Registry>,
) -> (
    Arc<Spec>,
    usize,
    Vec<OptionSpec>,
    Vec<OptionSpec>,
    ParserDirectives,
    bool,
) {
    let mut current = root;
    apply_generate_spec(&mut current, tokens);
    let mut index = 1;
    let mut path_end = 1usize;
    let mut positional = 0usize;
    let mut after_double_dash = false;
    let mut persistent_options = merge_persistent_options(&[], current.as_ref());
    let mut passed_options = Vec::new();
    let mut parser_directives = current.parser_directives.clone().unwrap_or_default();
    let mut option_arg = None;
    let mut entered_args = false;
    let mut subcommand_variadic_count = 0usize;
    while index < limit {
        let token = &tokens[index];
        let persistent_refs: Vec<&OptionSpec> = persistent_options.iter().collect();
        let subcommand_arg = positional_arg(&current.args, positional);
        let options_allowed = can_consume_options(
            &parser_directives,
            after_double_dash,
            entered_args,
            option_arg.as_ref(),
            subcommand_arg,
            subcommand_variadic_count,
        );
        let subcommands_allowed = can_consume_subcommands(option_arg.as_ref(), entered_args);
        if !after_double_dash && token == "--" && options_allowed {
            after_double_dash = true;
            index += 1;
            continue;
        }
        if !after_double_dash && token.starts_with('-') && options_allowed {
            if let Some(resolved) =
                resolve_option_token_with_persistent(current.as_ref(), &persistent_refs, token, &parser_directives)
            {
                let option = resolved.option.clone();
                for passed in options_in_token(current.as_ref(), &persistent_refs, token, &parser_directives) {
                    passed_options.push(passed.clone());
                }
                if let Some(arg) = option.args.first() {
                    if let Some(attached) = resolved.attached_value {
                        option_arg = arg.is_variadic.then(|| OptionArgState {
                            arg: arg.clone(),
                            count: 1,
                        });
                        index += 1;
                        if let Some(next) = next_spec_after_arg(registry.as_deref_mut(), arg, attached) {
                            enter_loaded_spec(
                                &mut current,
                                &mut path_end,
                                &mut positional,
                                &mut persistent_options,
                                &mut parser_directives,
                                &mut passed_options,
                                &mut option_arg,
                                &mut entered_args,
                                &mut subcommand_variadic_count,
                                next,
                                index,
                                tokens,
                            );
                        }
                        continue;
                    }
                    if index + 1 < limit
                        && (should_consume_option_value(
                            current.as_ref(),
                            &option,
                            &tokens[index + 1],
                            &parser_directives,
                        ) || !arg.is_optional)
                    {
                        let value = tokens[index + 1].clone();
                        option_arg = arg.is_variadic.then(|| OptionArgState {
                            arg: arg.clone(),
                            count: 1,
                        });
                        index += 2;
                        if let Some(next) = next_spec_after_arg(registry.as_deref_mut(), arg, &value) {
                            enter_loaded_spec(
                                &mut current,
                                &mut path_end,
                                &mut positional,
                                &mut persistent_options,
                                &mut parser_directives,
                                &mut passed_options,
                                &mut option_arg,
                                &mut entered_args,
                                &mut subcommand_variadic_count,
                                next,
                                index,
                                tokens,
                            );
                        }
                        continue;
                    }
                    option_arg = Some(OptionArgState {
                        arg: arg.clone(),
                        count: 0,
                    });
                }
                index += 1;
                continue;
            }
        }
        if option_arg.is_some() {
            let state = option_arg.expect("checked above");
            option_arg = state.arg.is_variadic.then_some(OptionArgState {
                arg: state.arg,
                count: state.count + 1,
            });
            index += 1;
            continue;
        }
        if !after_double_dash && subcommands_allowed && !token.starts_with('-') {
            if let Some(next) = current.find_subcommand(token) {
                current = Arc::new(next.clone());
                apply_generate_spec(&mut current, tokens);
                index += 1;
                path_end = index;
                positional = 0;
                persistent_options = merge_persistent_options(&persistent_options, current.as_ref());
                parser_directives = inherit_subcommand_directives(&parser_directives, current.as_ref());
                passed_options.clear();
                entered_args = false;
                subcommand_variadic_count = 0;
                continue;
            }
        }
        // A completed positional argument can replace the active spec.  The
        // boundary moves past that token so the loaded spec does not recount
        // the value as its own first positional argument.
        if let Some(arg) = subcommand_arg {
            entered_args = true;
            if arg.is_variadic {
                subcommand_variadic_count += 1;
            } else {
                positional += 1;
            }
            index += 1;
            if let Some(next) = next_spec_after_arg(registry.as_deref_mut(), arg, token) {
                enter_loaded_spec(
                    &mut current,
                    &mut path_end,
                    &mut positional,
                    &mut persistent_options,
                    &mut parser_directives,
                    &mut passed_options,
                    &mut option_arg,
                    &mut entered_args,
                    &mut subcommand_variadic_count,
                    next,
                    index,
                    tokens,
                );
            }
            continue;
        }
        positional += 1;
        index += 1;
    }
    let options_allowed = can_consume_options(
        &parser_directives,
        after_double_dash,
        entered_args,
        option_arg.as_ref(),
        positional_arg(&current.args, positional),
        subcommand_variadic_count,
    );
    (
        current,
        path_end,
        persistent_options,
        passed_options,
        parser_directives,
        options_allowed,
    )
}

fn apply_generate_spec(current: &mut Arc<Spec>, tokens: &[String]) {
    let Some(hook_id) = current.js_generate_spec.clone() else {
        return;
    };
    let Some((host, cwd)) = crate::js_host::current() else {
        return;
    };
    let timeout = Duration::from_millis(u64::try_from(crate::generate::DEFAULT_SCRIPT_TIMEOUT_MS).unwrap_or(5_000));
    let generated = if let Some(key) = current.generate_spec_cache_key.as_deref() {
        let cache_key = format!("{}:{key}", tokens.first().cloned().unwrap_or_default());
        crate::js_host::cached_spec(host, &cache_key, || host.generate_spec(&hook_id, tokens, cwd, timeout))
    } else {
        host.generate_spec(&hook_id, tokens, cwd, timeout)
    };
    let Some(generated) = generated else {
        return;
    };
    *current = Arc::new(crate::js_host::merge_generated_spec(current.as_ref(), generated));
}

#[allow(clippy::too_many_arguments)]
fn enter_loaded_spec(
    current: &mut Arc<Spec>,
    path_end: &mut usize,
    positional: &mut usize,
    persistent_options: &mut Vec<OptionSpec>,
    parser_directives: &mut ParserDirectives,
    passed_options: &mut Vec<OptionSpec>,
    option_arg: &mut Option<OptionArgState>,
    entered_args: &mut bool,
    subcommand_variadic_count: &mut usize,
    next: Arc<Spec>,
    index: usize,
    tokens: &[String],
) {
    *current = next;
    apply_generate_spec(current, tokens);
    *path_end = index;
    *positional = 0;
    *persistent_options = merge_persistent_options(persistent_options, current.as_ref());
    *parser_directives = merge_parser_directives(parser_directives, current.as_ref());
    passed_options.clear();
    *option_arg = None;
    *entered_args = false;
    *subcommand_variadic_count = 0;
}

/// Fig prefers a static `loadSpec` on the argument. Only when that is absent
/// do `isCommand` / `isScript` / `isModule` load another bundled spec.
fn next_spec_after_arg(registry: Option<&mut Registry>, arg: &ArgSpec, token: &str) -> Option<Arc<Spec>> {
    if arg.load_spec.is_some() {
        return arg.resolved_spec.as_deref().map(|spec| Arc::new(spec.clone()));
    }
    if let Some(resolved) = arg.resolved_spec.as_deref() {
        return Some(Arc::new(resolved.clone()));
    }
    let name = dynamic_spec_name(arg, token)?;
    if name.is_empty() || name == "?" {
        return None;
    }
    registry.and_then(|registry| registry.get_arc(&name))
}

fn dynamic_spec_name(arg: &ArgSpec, token: &str) -> Option<String> {
    if let Some(prefix) = arg.is_module.as_deref() {
        if prefix.is_empty() {
            return None;
        }
        return Some(format!("{prefix}{token}"));
    }
    if arg.is_command || arg.is_script {
        return Some(command_lookup_name(token, arg.is_script));
    }
    None
}

fn command_lookup_name(token: &str, is_script: bool) -> String {
    let pathish = token.starts_with('/') || token.starts_with("./") || token.starts_with("~/");
    if is_script || pathish {
        token
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(token)
            .to_string()
    } else {
        token.to_string()
    }
}

fn arg_suggests_commands(arg: &ArgSpec) -> bool {
    arg.is_command || arg.is_script || arg.is_module.as_ref().is_some_and(|prefix| !prefix.is_empty())
}

fn command_arg_suggestions(registry: &Registry, arg: &ArgSpec, query: &str, fuzzy: bool) -> Vec<Suggestion> {
    if let Some(prefix) = arg.is_module.as_deref().filter(|prefix| !prefix.is_empty()) {
        let needle = format!("{prefix}{query}");
        return registry
            .command_names_matching_including_exact_with(&needle, fuzzy)
            .into_iter()
            .filter_map(|(name, description)| {
                let display = name.strip_prefix(prefix)?.to_string();
                if display.is_empty() {
                    return None;
                }
                Some(Suggestion::new(display.clone(), description, "arg").with_insert_value(display))
            })
            .collect();
    }
    registry
        .command_names_matching_including_exact_with(query, fuzzy)
        .into_iter()
        .map(|(name, description)| Suggestion::new(name.clone(), description, "arg").with_insert_value(name))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn active_arg<'a>(
    spec: &'a Spec,
    persistent_options: &[&'a OptionSpec],
    tokens: &[String],
    path_end: usize,
    limit: usize,
    ends_with_space: bool,
    query: &str,
    raw_query: &str,
    parser_directives: &ParserDirectives,
) -> Option<ActiveArg> {
    let current_index = if ends_with_space {
        tokens.len()
    } else {
        tokens.len().saturating_sub(1)
    };
    let mut positional = 0usize;
    let mut index = path_end;
    let mut option_arg = None;
    let mut entered_args = false;
    let mut subcommand_variadic_count = 0usize;
    // A loadSpec transition may move `path_end` past the `--` sentinel. Keep
    // that parser state when deciding whether a leading dash is positional.
    let mut after_double_dash = tokens.iter().take(path_end).any(|token| token == "--");

    while index < limit {
        let token = &tokens[index];
        let subcommand_arg = positional_arg(&spec.args, positional);
        let options_allowed = can_consume_options(
            parser_directives,
            after_double_dash,
            entered_args,
            option_arg.as_ref(),
            subcommand_arg,
            subcommand_variadic_count,
        );
        if !after_double_dash && token == "--" && options_allowed {
            after_double_dash = true;
            index += 1;
            continue;
        }
        if !after_double_dash && token.starts_with('-') && options_allowed {
            if let Some(resolved) =
                resolve_option_token_with_persistent(spec, persistent_options, token, parser_directives)
            {
                let option = resolved.option;
                option_arg = None;
                if let Some(value) = resolved.attached_value {
                    if !ends_with_space && index == current_index {
                        let raw_value = attached_search_term(raw_query, resolved, value);
                        return option.args.first().map(|arg| ActiveArg {
                            arg: arg.clone(),
                            query: value.to_string(),
                            search_term: raw_value,
                            exclusive: true,
                        });
                    }
                    if let Some(arg) = option.args.first().filter(|arg| arg.is_variadic) {
                        option_arg = Some(OptionArgState {
                            arg: arg.clone(),
                            count: 1,
                        });
                    }
                    index += 1;
                    continue;
                }
                if !option.args.is_empty() {
                    if index + 1 < limit
                        && (should_consume_option_value(spec, option, &tokens[index + 1], parser_directives)
                            || option.args.first().is_some_and(|arg| !arg.is_optional))
                    {
                        option_arg = option
                            .args
                            .first()
                            .filter(|arg| arg.is_variadic)
                            .map(|arg| OptionArgState {
                                arg: arg.clone(),
                                count: 1,
                            });
                        // The next completed token is the option argument.
                        index += 2;
                        continue;
                    }
                    // The token under the caret (or the empty token after a
                    // trailing space) is this option's argument.  Do not
                    // consume it as a positional argument.
                    return option.args.first().map(|arg| ActiveArg {
                        arg: arg.clone(),
                        query: query.to_string(),
                        search_term: raw_query.to_string(),
                        exclusive: !arg.is_optional,
                    });
                }
            }
        }
        if let Some(state) = option_arg {
            option_arg = Some(OptionArgState {
                arg: state.arg,
                count: state.count + 1,
            });
            index += 1;
            continue;
        }
        if let Some(arg) = subcommand_arg {
            if arg.is_variadic {
                subcommand_variadic_count += 1;
            } else {
                positional += 1;
            }
        } else {
            positional += 1;
        }
        entered_args = true;
        index += 1;
    }

    if let Some(state) = option_arg {
        return Some(ActiveArg {
            arg: state.arg,
            query: query.to_string(),
            search_term: raw_query.to_string(),
            exclusive: true,
        });
    }

    // With no trailing space the current token is intentionally excluded from
    // the consumed-token scan.  An attached option value (`--opt=value`) is
    // the one exception: inspect that raw token so its value arg remains the
    // active context and only `value` becomes the search term.
    if !ends_with_space {
        if let Some(token) = tokens.get(current_index) {
            let subcommand_arg = positional_arg(&spec.args, positional);
            let options_allowed = can_consume_options(
                parser_directives,
                after_double_dash,
                entered_args,
                option_arg.as_ref(),
                subcommand_arg,
                subcommand_variadic_count,
            );
            if options_allowed {
                if let Some(resolved) =
                    resolve_option_token_with_persistent(spec, persistent_options, token, parser_directives)
                {
                    let option = resolved.option;
                    if let Some(value) = resolved.attached_value {
                        let raw_value = attached_search_term(raw_query, resolved, value);
                        if let Some(arg) = option.args.first() {
                            return Some(ActiveArg {
                                arg: arg.clone(),
                                query: value.to_string(),
                                search_term: raw_value,
                                exclusive: true,
                            });
                        }
                    }
                    // The WebView treats the last short option in a composite
                    // token as the active option. Its argument rows use an empty
                    // query, so accepting one appends to `-am` instead of deleting
                    // the chain. A lone `-m` keeps the ordinary option path.
                    if resolved.chain.is_some()
                        && let Some(arg) = option.args.first()
                    {
                        return Some(ActiveArg {
                            arg: arg.clone(),
                            query: String::new(),
                            search_term: String::new(),
                            exclusive: !arg.is_optional,
                        });
                    }
                }
            }
            if token.starts_with('-') && !after_double_dash && options_allowed {
                // The token is still an option/sentinel query, not a value
                // for the command's positional arg.  Once `--` has been
                // completed, a leading dash is a legitimate positional value.
                return None;
            }
        }
    }

    positional_arg(&spec.args, positional).map(|arg| ActiveArg {
        arg: arg.clone(),
        query: query.to_string(),
        search_term: raw_query.to_string(),
        exclusive: false,
    })
}

fn positional_arg(args: &[ArgSpec], index: usize) -> Option<&ArgSpec> {
    let arg = args.get(index).or_else(|| args.last().filter(|arg| arg.is_variadic))?;
    if arg.name.is_empty()
        && arg.description.is_empty()
        && arg.builtin.is_none()
        && arg.builtins.is_empty()
        && arg.script.is_empty()
        && arg.templates.is_empty()
        && arg.suggestions.is_empty()
        && !arg.is_command
        && !arg.is_script
        && arg.is_module.is_none()
    {
        // An empty placeholder is still a real positional slot when it is
        // variadic; otherwise there is no useful current-arg context.
        return arg.is_variadic.then_some(arg);
    }
    Some(arg)
}

fn merge_persistent_options(parent: &[OptionSpec], spec: &Spec) -> Vec<OptionSpec> {
    let mut options = parent.to_vec();
    for option in &spec.persistent_options {
        if let Some(existing) = options.iter_mut().find(|existing| options_are_equal(existing, option)) {
            *existing = option.clone();
        } else {
            options.push(option.clone());
        }
    }
    options
}

fn options_are_equal(a: &OptionSpec, b: &OptionSpec) -> bool {
    a.names
        .iter()
        .any(|name| b.names.iter().any(|candidate| candidate == name))
}

fn separator_at(token: &str, directives: &ParserDirectives) -> Option<(usize, char)> {
    token.char_indices().find(|(_, character)| {
        directives
            .option_arg_separators
            .as_ref()
            .map_or(*character == '=', |separators| {
                separators
                    .iter()
                    .any(|separator| separator.chars().count() == 1 && separator.starts_with(*character))
            })
    })
}

fn find_option_in<'a>(
    spec: &'a Spec,
    persistent: &[&'a OptionSpec],
    token: &str,
    directives: &ParserDirectives,
) -> Option<&'a OptionSpec> {
    let option_name = separator_at(token, directives).map_or(token, |(index, _)| &token[..index]);
    spec.options
        .iter()
        .find(|option| option.names.iter().any(|name| name == option_name))
        .or_else(|| {
            persistent
                .iter()
                .copied()
                .find(|option| option.names.iter().any(|name| name == option_name))
        })
}

fn short_option_name(option: &OptionSpec) -> Option<&str> {
    option.names.iter().find_map(|name| {
        let rest = name.strip_prefix('-')?;
        (!rest.starts_with('-') && rest.chars().count() == 1).then_some(name.as_str())
    })
}

fn find_short_option_in<'a>(spec: &'a Spec, persistent: &[&'a OptionSpec], letter: char) -> Option<&'a OptionSpec> {
    spec.options.iter().chain(persistent.iter().copied()).find(|option| {
        option.names.iter().any(|name| {
            let Some(rest) = name.strip_prefix('-') else {
                return false;
            };
            !rest.starts_with('-') && {
                let mut chars = rest.chars();
                chars.next() == Some(letter) && chars.next().is_none()
            }
        })
    })
}

fn parse_short_option_chain<'spec, 'token>(
    spec: &'spec Spec,
    persistent: &[&'spec OptionSpec],
    token: &'token str,
    directives: &ParserDirectives,
) -> Option<ShortOptionChain<'spec, 'token>> {
    if directives.flags_are_posix_noncompliant == Some(true)
        || !token.starts_with('-')
        || token.starts_with("--")
        || token[1..].chars().count() < 2
    {
        return None;
    }

    let mut last = None;
    for (offset, letter) in token[1..].char_indices() {
        let option = find_short_option_in(spec, persistent, letter)?;
        let end = 1 + offset + letter.len_utf8();
        let remainder = &token[end..];
        last = Some((option, end));

        if option.args.is_empty() || remainder.is_empty() {
            continue;
        }

        let mandatory = option.args.first().is_some_and(|arg| !arg.is_optional);
        let next_is_short_option = remainder
            .chars()
            .next()
            .and_then(|next| find_short_option_in(spec, persistent, next))
            .is_some();
        if mandatory || remainder.starts_with('=') || !next_is_short_option {
            return Some(ShortOptionChain {
                option,
                prefix: &token[..end],
                attached_value: Some(remainder.strip_prefix('=').unwrap_or(remainder)),
            });
        }
    }

    let (option, end) = last?;
    Some(ShortOptionChain {
        option,
        prefix: &token[..end],
        attached_value: None,
    })
}

fn resolve_option_token_with_persistent<'spec, 'token>(
    spec: &'spec Spec,
    persistent: &[&'spec OptionSpec],
    token: &'token str,
    directives: &ParserDirectives,
) -> Option<ResolvedOptionToken<'spec, 'token>> {
    let long_form = directives.flags_are_posix_noncompliant == Some(true) || token.starts_with("--");
    // In POSIX mode a multi-letter single-dash token is parsed as a short
    // option chain before exact lookup.  This is observable when a spec has
    // both `-word` and `-w`/`-o` options: the WebView consumes the chain.
    if !long_form && token.starts_with('-') && token.len() > 1 && token[1..].chars().count() >= 2 {
        if let Some(chain) = parse_short_option_chain(spec, persistent, token, directives) {
            return Some(ResolvedOptionToken {
                option: chain.option,
                attached_value: chain.attached_value,
                attached_separator: None,
                chain: Some(chain),
            });
        }
    }
    let option = find_option_in(spec, persistent, token, directives)?;
    let (attached_separator, attached_value) = separator_at(token, directives)
        .map_or((None, None), |(index, separator)| {
            (Some(separator), Some(&token[index + separator.len_utf8()..]))
        });
    Some(ResolvedOptionToken {
        option,
        attached_value: option.args.first().and(attached_value),
        attached_separator: option.args.first().and(attached_separator),
        chain: None,
    })
}

/// Every option represented by a completed token. The active-argument walker
/// only needs the final option in a short chain, while `passedOptions` counts
/// each flag (for example both `-a` and `-b` in `-ab`).
fn options_in_token<'a>(
    spec: &'a Spec,
    persistent: &[&'a OptionSpec],
    token: &str,
    directives: &ParserDirectives,
) -> Vec<&'a OptionSpec> {
    if token == "--" || !token.starts_with('-') {
        return Vec::new();
    }
    if token.starts_with("--") || directives.flags_are_posix_noncompliant == Some(true) {
        return find_option_in(spec, persistent, token, directives)
            .into_iter()
            .collect();
    }

    let mut options = Vec::new();
    let chars = token[1..].char_indices();
    for (offset, letter) in chars {
        let Some(option) = find_short_option_in(spec, persistent, letter) else {
            break;
        };
        options.push(option);
        let end = offset + letter.len_utf8();
        let remainder = &token[1 + end..];
        if option.args.first().is_some_and(|arg| !arg.is_optional)
            || separator_at(remainder, directives).is_some()
            || remainder
                .chars()
                .next()
                .is_some_and(|next| find_short_option_in(spec, persistent, next).is_none())
        {
            break;
        }
    }
    options
}

fn attached_search_term(raw_query: &str, resolved: ResolvedOptionToken<'_, '_>, fallback: &str) -> String {
    if let Some(chain) = resolved.chain {
        return raw_query.strip_prefix(chain.prefix).map_or_else(
            || fallback.to_string(),
            |value| value.strip_prefix('=').unwrap_or(value).to_string(),
        );
    }
    resolved.attached_separator.map_or_else(
        || fallback.to_string(),
        |separator| {
            raw_query.find(separator).map_or_else(
                || fallback.to_string(),
                |index| raw_query[index + separator.len_utf8()..].to_string(),
            )
        },
    )
}

fn should_consume_option_value(spec: &Spec, option: &OptionSpec, next: &str, _directives: &ParserDirectives) -> bool {
    if next == "--" || next.starts_with('-') {
        return false;
    }
    if option.args.first().is_some_and(|arg| !arg.is_optional) {
        return true;
    }
    // Optional option arguments are ambiguous.  Prefer a child subcommand
    // when one is available, otherwise treat the next token as the value.
    spec.find_subcommand(next).is_none()
}

fn first_token_result(
    registry: &mut Registry,
    request: &CompleteRequest,
    raw_search_term: String,
    normalized_search_term: String,
) -> CompleteResult {
    if !request.suggest_first_token {
        return CompleteResult {
            suggestions: Vec::new(),
            fuzzy: request.fuzzy,
            search_term: raw_search_term,
            match_term: normalized_search_term,
            current_arg: None,
        };
    }
    let mut suggestions = Vec::new();
    for (name, description) in
        registry.command_names_matching_including_exact_with(&normalized_search_term, request.fuzzy)
    {
        // The legacy first-token generator returned ordinary `arg` rows
        // with an explicit raw insertValue and no shouldAddSpace flag.
        // Keeping that shape matters: accepting `git` must insert only
        // the missing token, while the optional auto-execute wrapper is
        // the only path that appends a newline.
        suggestions.push(Suggestion::new(name.clone(), description, "arg").with_insert_value(name));
    }
    add_exact_auto_execute(
        &mut suggestions,
        &normalized_search_term,
        fig_settings::settings::get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        fig_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false),
        fig_settings::settings::get_bool_or("autocomplete.immediatelyRunDangerousCommands", false),
    );
    CompleteResult {
        suggestions,
        fuzzy: request.fuzzy,
        search_term: raw_search_term,
        match_term: normalized_search_term,
        current_arg: None,
    }
}

pub fn complete(registry: &mut Registry, request: &CompleteRequest) -> CompleteResult {
    let raw = buffer_before_cursor(&request.buffer, request.cursor);
    let buffer = completion_buffer(&request.buffer, request.cursor);
    let (tokens, ends_with_space) = tokenize(buffer);
    if tokens.is_empty() {
        if is_fresh_command_position(raw) {
            return first_token_result(registry, request, String::new(), String::new());
        }
        return CompleteResult {
            fuzzy: request.fuzzy,
            ..CompleteResult::default()
        };
    }

    let command = &tokens[0];
    let raw_search_term = if ends_with_space {
        String::new()
    } else {
        current_token_raw(buffer)
    };
    let normalized_search_term = if ends_with_space {
        String::new()
    } else {
        tokens.last().cloned().unwrap_or_default()
    };
    let prefer_verbose = fig_settings::settings::get_bool_or("autocomplete.preferVerboseSuggestions", false);

    if command_is_disabled(command) {
        return CompleteResult {
            suggestions: Vec::new(),
            fuzzy: request.fuzzy,
            search_term: raw_search_term,
            match_term: normalized_search_term,
            current_arg: None,
        };
    }

    if tokens.len() == 1 && !ends_with_space {
        return first_token_result(registry, request, raw_search_term, normalized_search_term);
    }

    let query = normalized_search_term.clone();

    let Some(root) = registry.get_arc(command) else {
        return CompleteResult {
            suggestions: filter_query(
                crate::cobra::complete(&tokens, &request.cwd, request.fuzzy),
                &query,
                request.fuzzy,
            ),
            fuzzy: request.fuzzy,
            search_term: raw_search_term,
            match_term: query,
            current_arg: None,
        };
    };

    let context = resolve_context(root, &tokens, ends_with_space, &query, &raw_search_term, Some(registry));
    let fuzzy = effective_fuzzy(
        request.fuzzy,
        Some(context.spec.as_ref()),
        context.active_arg.as_ref().map(|active| &active.arg),
    );
    let current = context.spec.as_ref();
    let persistent_refs: Vec<&OptionSpec> = context.persistent_options.iter().collect();
    let option_chain = (!ends_with_space)
        .then(|| {
            parse_short_option_chain(
                current,
                &persistent_refs,
                &normalized_search_term,
                &context.parser_directives,
            )
        })
        .flatten();
    let open_option_chain = option_chain.filter(|chain| chain.attached_value.is_none());
    let (mut query, search_term) = context.active_arg.as_ref().map_or_else(
        || (query.clone(), raw_search_term.clone()),
        |active| (active.query.clone(), active.search_term.clone()),
    );
    // A generator-level string getQueryTerm changes the term used by every
    // row it returns.  Per-suggestion overrides are applied independently in
    // collect_named/generate_arg below.
    if let Some(active) = context.active_arg.as_ref() {
        if active.arg.meta.get_query_term.is_some() {
            query = query_term_for(&search_term, active.arg.meta.get_query_term.as_deref());
        }
    }
    let current_arg = context.active_arg.as_ref().map(|active| CurrentArg {
        name: active.arg.name.clone(),
        description: active.arg.description.clone(),
    });

    let completing_exclusive_arg = context.active_arg.as_ref().is_some_and(|active| active.exclusive);
    let mut subcommands = current.subcommands.clone();
    subcommands.sort_by(|left, right| cmp_named_names(&left.names, &right.names));
    let mut suggestions = if completing_exclusive_arg || open_option_chain.is_some() {
        Vec::new()
    } else {
        collect_named(
            &subcommands,
            |spec| spec.names.as_slice(),
            |spec| spec.description.as_str(),
            |spec| args_hint(&spec.args),
            |spec| &spec.meta,
            |spec| {
                spec.meta.should_add_space.unwrap_or_else(|| {
                    should_add_space(&spec.args, spec.requires_subcommand, !spec.subcommands.is_empty())
                })
            },
            |spec| spec.meta.separator_to_add.clone(),
            |spec| spec.args.first().is_some_and(|arg| !arg.is_optional),
            "subcommand",
            &query,
            &search_term,
            fuzzy,
            prefer_verbose,
        )
    };
    // Fig presents argument/generator results before additional shortcuts and
    // options. Keep that ordering so a generated git alias does not jump
    // behind the static option list while ranking is still settling.
    if let Some(active) = context.active_arg.as_ref() {
        if arg_suggests_commands(&active.arg) {
            suggestions.extend(command_arg_suggestions(registry, &active.arg, &query, fuzzy));
        }
        let mut active_suggestions = crate::generate::generate_for_arg_with_search_term(
            &active.arg,
            &tokens,
            &query,
            &search_term,
            &request.cwd,
            fuzzy,
        );
        if open_option_chain.is_some() {
            for suggestion in &mut active_suggestions {
                suggestion.query_term = Some(String::new());
            }
        }
        suggestions.extend(active_suggestions);
    }
    let mut additional_items = current.additional_suggestions.clone();
    additional_items.sort_by(|left, right| cmp_named_names(&left.names, &right.names));
    let mut additional = collect_named(
        &additional_items,
        |seed| seed.names.as_slice(),
        |seed| seed.description.as_str(),
        |seed| seed.args_hint.clone(),
        |seed| &seed.meta,
        |seed| seed.meta.should_add_space.unwrap_or(false),
        |seed| seed.meta.separator_to_add.clone(),
        |_| false,
        "arg",
        if open_option_chain.is_some() { "" } else { &query },
        if open_option_chain.is_some() { "" } else { &search_term },
        fuzzy,
        prefer_verbose,
    );
    if open_option_chain.is_some() {
        for suggestion in &mut additional {
            suggestion.query_term = Some(String::new());
        }
    }
    // `getStaticSuggestions` gives untyped additional suggestions the same
    // template tile used by the WebView, with a right-arrow badge. Preserve
    // that affordance while leaving explicitly typed/iconed rows untouched.
    for suggestion in &mut additional {
        let untyped_seed = current.additional_suggestions.iter().any(|seed| {
            seed.meta.suggestion_type.is_none()
                && seed.meta.icon.is_none()
                && seed.names.iter().any(|name| name == &suggestion.name)
        });
        if untyped_seed && suggestion.icon.is_none() {
            suggestion.icon = Some("fig://template?color=628dad&badge=➡️".into());
        }
    }
    suggestions.extend(additional);
    let include_options =
        context.options_allowed && !completing_exclusive_arg && (query.is_empty() || query.starts_with('-'));
    if let Some(chain) = open_option_chain {
        suggestions.extend(option_chain_suggestions(
            current,
            &persistent_refs,
            chain,
            &context.passed_options,
            &context.parser_directives,
        ));
    } else if include_options {
        suggestions.extend(collect_option_suggestions(
            current,
            &persistent_refs,
            &context.passed_options,
            &query,
            &search_term,
            fuzzy,
            prefer_verbose,
            &context.parser_directives,
        ));
    }

    if suggestions.is_empty() && context.active_arg.is_none() {
        suggestions.extend(filter_query(
            crate::cobra::complete(&tokens, &request.cwd, fuzzy),
            &query,
            fuzzy,
        ));
    }

    add_exact_auto_execute(
        &mut suggestions,
        &query,
        fig_settings::settings::get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        fig_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false),
        fig_settings::settings::get_bool_or("autocomplete.immediatelyRunDangerousCommands", false),
    );
    add_current_token_auto_execute(
        &mut suggestions,
        &normalized_search_term,
        suggest_current_token_for(
            context.active_arg.as_ref(),
            fig_settings::settings::get_bool_or("autocomplete.alwaysSuggestCurrentToken", false),
        ),
        fig_settings::settings::get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        fig_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false),
        fig_settings::settings::get_bool_or("autocomplete.immediatelyRunDangerousCommands", false),
    );
    add_space_auto_execute(
        &mut suggestions,
        &normalized_search_term,
        fig_settings::settings::get_bool_or("autocomplete.immediatelyExecuteAfterSpace", false),
        fig_settings::settings::get_bool_or("autocomplete.hideAutoExecuteSuggestion", false),
        fig_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false),
    );

    CompleteResult {
        suggestions,
        fuzzy,
        search_term,
        match_term: query,
        current_arg,
    }
}

pub fn buffer_before_cursor(buffer: &str, cursor: Option<u32>) -> &str {
    let Some(cursor) = cursor else {
        return buffer;
    };
    let mut end = (cursor as usize).min(buffer.len());
    while end > 0 && !buffer.is_char_boundary(end) {
        end -= 1;
    }
    &buffer[..end]
}

pub fn args_hint(args: &[ArgSpec]) -> String {
    args.iter()
        .filter(|arg| !arg.name.is_empty())
        .map(|arg| {
            let mut base = arg.name.clone();
            if arg.is_variadic {
                base.push_str("...");
            }
            if arg.is_optional {
                format!("[{base}]")
            } else {
                format!("<{base}>")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn filter_query(items: Vec<Suggestion>, query: &str, fuzzy: bool) -> Vec<Suggestion> {
    if query.is_empty() {
        return items;
    }
    items
        .into_iter()
        .filter(|item| matches_query(&item.name, query, fuzzy))
        .collect()
}

fn select_named_candidate<'a>(
    item_names: &'a [String],
    display_name: Option<&str>,
    query: &str,
    fuzzy: bool,
    prefer_long_name: bool,
) -> Option<&'a String> {
    let matching_names = item_names
        .iter()
        .filter(|candidate| matches_query(candidate, query, fuzzy))
        .collect::<Vec<_>>();
    if matching_names.is_empty() {
        display_name
            .filter(|display| matches_query(display, query, fuzzy))
            .and_then(|_| {
                if prefer_long_name {
                    item_names.iter().max_by_key(|candidate| candidate.len())
                } else {
                    item_names.first()
                }
            })
    } else if prefer_long_name {
        matching_names.into_iter().max_by_key(|candidate| candidate.len())
    } else {
        matching_names.into_iter().next()
    }
}

fn hidden_item_is_visible(hidden: bool, item_names: &[String], query: &str) -> bool {
    !hidden || item_names.iter().any(|candidate| candidate.eq_ignore_ascii_case(query))
}

fn suggest_current_token_for(active_arg: Option<&ActiveArg>, global: bool) -> bool {
    active_arg
        .and_then(|active| active.arg.suggest_current_token)
        .unwrap_or(global)
}

/// Resolve the filtering mode with the same precedence as the WebView. An
/// active argument owns the decision: an omitted argument strategy falls back
/// directly to the user's setting and must not inherit its parent spec's
/// strategy. Only when there is no active argument does the current spec's
/// strategy apply.
pub(crate) fn effective_fuzzy(user_fuzzy: bool, spec: Option<&Spec>, active_arg: Option<&ArgSpec>) -> bool {
    if let Some(strategy) = active_arg.and_then(|arg| arg.filter_strategy) {
        return strategy.effective_fuzzy(user_fuzzy);
    }
    if active_arg.is_some() {
        return user_fuzzy;
    }
    spec.and_then(|spec| spec.filter_strategy)
        .map_or(user_fuzzy, |strategy| strategy.effective_fuzzy(user_fuzzy))
}

pub(crate) fn effective_fuzzy_for_tokens(
    registry: &mut Registry,
    user_fuzzy: bool,
    tokens: &[String],
    ends_with_space: bool,
    query: &str,
    raw_query: &str,
) -> bool {
    let Some(command) = tokens.first() else {
        return user_fuzzy;
    };
    let Some(root) = registry.get_arc(command) else {
        return user_fuzzy;
    };
    let context = resolve_context(root, tokens, ends_with_space, query, raw_query, Some(registry));
    effective_fuzzy(
        user_fuzzy,
        Some(context.spec.as_ref()),
        context.active_arg.as_ref().map(|active| &active.arg),
    )
}

#[allow(clippy::too_many_arguments)]
fn collect_named<T>(
    items: &[T],
    names: impl Fn(&T) -> &[String],
    description: impl Fn(&T) -> &str,
    hint: impl Fn(&T) -> String,
    meta: impl Fn(&T) -> &SuggestionMeta,
    should_add_space: impl Fn(&T) -> bool,
    separator: impl Fn(&T) -> Option<String>,
    requires_arg: impl Fn(&T) -> bool,
    kind: &str,
    query: &str,
    search_term: &str,
    fuzzy: bool,
    prefer_verbose: bool,
) -> Vec<Suggestion> {
    let mut out = Vec::new();
    for item in items {
        let item_names = names(item);
        let metadata = meta(item);
        let suggestion_kind = metadata.suggestion_type.as_deref().unwrap_or(kind);
        let (item_query, item_query_term) =
            suggestion_query_term(suggestion_kind, metadata.get_query_term.as_deref(), query, search_term);
        let prefer_long_name = prefer_verbose && matches!(suggestion_kind, "option" | "subcommand");
        let name = select_named_candidate(
            item_names,
            metadata.display_name.as_deref(),
            &item_query,
            fuzzy,
            prefer_long_name,
        );
        let Some(name) = name else {
            continue;
        };
        // Hidden rows are omitted for empty/partial searches, but the old
        // filter deliberately revealed them when an alias was typed exactly.
        // `displayName` never counted as that exact-name exception.
        if !hidden_item_is_visible(metadata.hidden, item_names, &item_query) {
            continue;
        }
        let display_name = metadata
            .display_name
            .clone()
            .or_else(|| (item_names.len() > 1).then(|| item_names.join(", ")));
        let mut suggestion = Suggestion::new(name.clone(), description(item), suggestion_kind)
            .with_args_hint(hint(item))
            .with_meta(
                metadata.insert_value.clone(),
                display_name,
                separator(item),
                should_add_space(item),
                metadata.hidden,
                metadata.priority,
                metadata.icon.clone(),
            )
            .with_primary_name(item_names.first().cloned())
            .with_dangerous(metadata.is_dangerous)
            .with_original_type(metadata.original_type.clone())
            .with_query_term(item_query_term);
        suggestion.requires_arg = requires_arg(item);
        out.push(suggestion);
    }
    out
}

fn cmp_named_names(left: &[String], right: &[String]) -> std::cmp::Ordering {
    let left = left.first().map(String::as_str).unwrap_or_default();
    let right = right.first().map(String::as_str).unwrap_or_default();
    crate::query::cmp_ignore_ascii_case(left, right).then_with(|| left.cmp(right))
}

fn option_repetition_limit(option: &OptionSpec) -> Option<f64> {
    match option.is_repeatable.as_ref() {
        None | Some(serde_json::Value::Null | serde_json::Value::Bool(false)) => Some(1.0),
        Some(serde_json::Value::Bool(true)) => None,
        Some(serde_json::Value::Number(number)) => {
            let value = number.as_f64()?;
            // Fig treats numeric zero as falsy and therefore as the default
            // one repetition. Negative values are truthy but cannot admit a
            // passed row, which is the natural count < limit result.
            Some(if value == 0.0 { 1.0 } else { value })
        },
        Some(_) => Some(1.0),
    }
}

fn option_repetition_count(option: &OptionSpec, passed: &[OptionSpec]) -> usize {
    passed
        .iter()
        .filter(|candidate| options_are_equal(option, candidate))
        .count()
}

fn option_is_excluded(option: &OptionSpec, passed: &[OptionSpec]) -> bool {
    passed.iter().any(|candidate| {
        candidate
            .exclusive_on
            .iter()
            .any(|excluded| option.names.iter().any(|name| name == excluded))
    })
}

fn option_priority(option: &OptionSpec, passed: &[OptionSpec]) -> Option<i64> {
    let mut unmet = HashSet::new();
    for candidate in passed {
        unmet.extend(candidate.depends_on.iter().cloned());
    }
    for candidate in passed {
        for name in &candidate.names {
            unmet.remove(name);
        }
    }
    option
        .names
        .iter()
        .any(|name| unmet.contains(name))
        .then_some(75)
        .or(option.meta.priority)
}

#[allow(clippy::too_many_arguments)]
fn collect_option_suggestions(
    current: &Spec,
    persistent: &[&OptionSpec],
    passed: &[OptionSpec],
    query: &str,
    search_term: &str,
    fuzzy: bool,
    prefer_verbose: bool,
    directives: &ParserDirectives,
) -> Vec<Suggestion> {
    let mut options = Vec::new();
    for option in current.options.iter().chain(persistent.iter().copied()) {
        if option_is_excluded(option, passed)
            || option_repetition_limit(option)
                .is_some_and(|limit| option_repetition_count(option, passed) as f64 >= limit)
        {
            continue;
        }
        let mut option = option.clone();
        option.meta.priority = option_priority(&option, passed);
        options.push(option);
    }
    options.sort_by(|left, right| cmp_named_names(&left.names, &right.names));
    collect_named(
        &options,
        |opt| opt.names.as_slice(),
        |opt| opt.description.as_str(),
        |opt| args_hint(&opt.args),
        |opt| &opt.meta,
        |opt| {
            opt.meta
                .should_add_space
                .unwrap_or_else(|| should_add_space(&opt.args, opt.requires_subcommand, false))
        },
        |opt| option_separator(opt, directives),
        |opt| opt.args.first().is_some_and(|arg| !arg.is_optional),
        "option",
        query,
        search_term,
        fuzzy,
        prefer_verbose,
    )
}

fn option_separator(option: &crate::ir::OptionSpec, directives: &ParserDirectives) -> Option<String> {
    if let Some(separator) = option.meta.separator_to_add.clone() {
        return Some(separator);
    }
    match option.requires_separator.as_ref() {
        Some(serde_json::Value::String(separator)) => Some(separator.clone()),
        Some(serde_json::Value::Bool(true)) if !option.args.first().is_some_and(|arg| arg.is_optional) => {
            match &directives.option_arg_separators {
                Some(separators) => separators.first().cloned(),
                None => Some("=".into()),
            }
        },
        _ if option.requires_equals && !option.args.first().is_some_and(|arg| arg.is_optional) => Some("=".into()),
        _ => None,
    }
}

/// Recreate the WebView's composite short-option rows. With `-ab` already in
/// the buffer, the current `-b` row is displayed as `-ab`; every other short
/// option becomes `-ab<letter>` and inserts only that extra letter. If the
/// last option requires an argument, keep only that current option alongside
/// the argument rows generated above.
fn option_chain_suggestions(
    spec: &Spec,
    persistent: &[&OptionSpec],
    chain: ShortOptionChain<'_, '_>,
    passed: &[OptionSpec],
    directives: &ParserDirectives,
) -> Vec<Suggestion> {
    let mandatory_arg = chain.option.args.first().is_some_and(|arg| !arg.is_optional);
    let mut suggestions = Vec::new();

    for option in spec.options.iter().chain(persistent.iter().copied()) {
        if option_is_excluded(option, passed)
            || option_repetition_limit(option)
                .is_some_and(|limit| option_repetition_count(option, passed) as f64 >= limit)
        {
            continue;
        }
        if mandatory_arg && !std::ptr::eq(option, chain.option) {
            continue;
        }
        let Some(short_name) = short_option_name(option) else {
            continue;
        };
        if option.meta.hidden {
            continue;
        }

        let is_current = std::ptr::eq(option, chain.option);
        let name = if is_current {
            chain.prefix.to_string()
        } else {
            format!("{}{}", chain.prefix, short_name.trim_start_matches('-'))
        };
        let display_name = if is_current {
            option.meta.display_name.clone().or_else(|| {
                (option.names.len() > 1).then(|| {
                    option
                        .names
                        .iter()
                        .map(|candidate| {
                            if candidate == short_name {
                                chain.prefix
                            } else {
                                candidate.as_str()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
            })
        } else {
            None
        };
        let should_add_space = option
            .meta
            .should_add_space
            .unwrap_or_else(|| should_add_space(&option.args, option.requires_subcommand, false));
        let kind = option.meta.suggestion_type.as_deref().unwrap_or("option");
        let mut suggestion = Suggestion::new(name.clone(), option.description.clone(), kind)
            .with_args_hint(args_hint(&option.args))
            .with_meta(
                if is_current {
                    option.meta.insert_value.clone()
                } else {
                    Some(name.clone())
                },
                display_name,
                option_separator(option, directives),
                should_add_space,
                option.meta.hidden,
                option_priority(option, passed),
                option.meta.icon.clone(),
            )
            .with_primary_name(if is_current {
                Some(chain.prefix.to_string())
            } else {
                Some(name.clone())
            })
            .with_dangerous(option.meta.is_dangerous)
            .with_original_type(option.meta.original_type.clone())
            .with_query_term(Some(chain.prefix.to_string()));
        suggestion.requires_arg = option.args.first().is_some_and(|arg| !arg.is_optional);
        suggestions.push(suggestion);
    }

    suggestions
}

fn should_add_space(args: &[ArgSpec], requires_subcommand: Option<bool>, has_subcommands: bool) -> bool {
    if args.first().is_some_and(|arg| !arg.is_optional) {
        return true;
    }
    requires_subcommand.unwrap_or(has_subcommands)
}

/// The WebView puts an auto-execute row immediately before an exact, safe
/// suggestion.  This is what makes Enter on `git status` execute the already
/// typed command instead of inserting an empty suffix.
fn add_exact_auto_execute(
    suggestions: &mut Vec<Suggestion>,
    query: &str,
    hide: bool,
    only_show_on_tab: bool,
    allow_dangerous: bool,
) {
    if query.is_empty() || hide || only_show_on_tab {
        return;
    }
    let Some(index) = suggestions.iter().position(|suggestion| {
        let row_query = suggestion.query_term.as_deref().unwrap_or(query);
        let exact_name = suggestion.name.eq_ignore_ascii_case(row_query)
            // File generators retain the trailing slash in the accepted name,
            // while the old WebView matched a folder query without it.
            || (suggestion.kind == "folder"
                && suggestion
                    .name
                    .strip_suffix('/')
                    .is_some_and(|name| name.eq_ignore_ascii_case(row_query)));
        suggestion.kind != "auto-execute"
            && exact_name
            && !suggestion.requires_arg
            && (allow_dangerous || !suggestion.is_dangerous)
            && suggestion
                .insert_value
                .as_deref()
                .is_none_or(|value| suggestion.primary_name.as_deref().unwrap_or(&suggestion.name) == value)
    }) else {
        return;
    };
    let original = suggestions[index].clone();
    let is_folder = original.kind == "folder";
    let auto = Suggestion {
        name: if is_folder {
            original.name.strip_suffix('/').unwrap_or(&original.name).to_string()
        } else {
            original.name.clone()
        },
        description: if is_folder {
            "folder".into()
        } else {
            original.description.clone()
        },
        kind: "auto-execute".into(),
        args_hint: original.args_hint,
        insert_value: Some("\n".into()),
        display_name: original.display_name,
        primary_name: original.primary_name,
        separator_to_add: None,
        should_add_space: false,
        hidden: false,
        // Keep this ahead of normal rows even after the native frecency sort.
        priority: i64::MAX,
        icon: Some("fig://icon?type=carrot".into()),
        original_type: original.original_type.clone().or_else(|| Some(original.kind.clone())),
        query_term: original.query_term,
        is_dangerous: false,
        requires_arg: false,
    };
    suggestions.insert(0, auto);
}

fn add_space_auto_execute(
    suggestions: &mut Vec<Suggestion>,
    query: &str,
    execute_after_space: bool,
    hide: bool,
    only_show_on_tab: bool,
) {
    if !query.is_empty() || !execute_after_space || hide || only_show_on_tab || suggestions.is_empty() {
        return;
    }
    suggestions.insert(
        0,
        Suggestion {
            name: "↪".into(),
            description: "Immediately execute".into(),
            kind: "auto-execute".into(),
            args_hint: String::new(),
            insert_value: Some("\n".into()),
            display_name: None,
            primary_name: None,
            separator_to_add: None,
            should_add_space: false,
            hidden: false,
            priority: i64::MAX,
            icon: Some("fig://icon?type=carrot".into()),
            original_type: None,
            query_term: None,
            is_dangerous: true,
            requires_arg: false,
        },
    );
}

#[allow(clippy::fn_params_excessive_bools)]
fn add_current_token_auto_execute(
    suggestions: &mut Vec<Suggestion>,
    query: &str,
    always_suggest_current_token: bool,
    hide: bool,
    only_show_on_tab: bool,
    allow_dangerous: bool,
) {
    if query.is_empty()
        || hide
        || only_show_on_tab
        || suggestions.is_empty()
        || suggestions.iter().any(|suggestion| suggestion.kind == "auto-execute")
    {
        return;
    }

    // `alwaysSuggestCurrentToken` is independent from the dangerous-command
    // setting for partial matches (for example `fo` -> `foo`).  The old
    // filter only blocked this fallback when the current token itself was an
    // exact dangerous command, so that Enter could not silently bypass the
    // safety guard.
    let dangerous_exact = !allow_dangerous
        && suggestions.iter().any(|suggestion| {
            let row_query = suggestion.query_term.as_deref().unwrap_or(query);
            let exact_name = suggestion.name.eq_ignore_ascii_case(row_query)
                || (suggestion.kind == "folder"
                    && suggestion
                        .name
                        .strip_suffix('/')
                        .is_some_and(|name| name.eq_ignore_ascii_case(row_query)));
            exact_name && suggestion.is_dangerous
        });
    if dangerous_exact {
        return;
    }

    // A directory query has a dedicated action in the old UI.  `.` is
    // special even when the generator returned files, while a trailing slash
    // is special only when a folder row is present (the Rust file generator
    // exposes that fact through `kind == "folder"`).
    let folder_query =
        query == "." || (query.ends_with('/') && suggestions.iter().any(|suggestion| suggestion.kind == "folder"));
    if folder_query {
        suggestions.insert(
            0,
            Suggestion {
                name: if query == "." { query.to_string() } else { "↪".into() },
                description: "Enter the current directory".into(),
                kind: "auto-execute".into(),
                args_hint: String::new(),
                insert_value: Some("\n".into()),
                display_name: None,
                primary_name: None,
                separator_to_add: None,
                should_add_space: false,
                hidden: false,
                priority: i64::MAX,
                icon: Some("fig://icon?type=carrot".into()),
                original_type: Some("folder".into()),
                query_term: None,
                is_dangerous: false,
                requires_arg: false,
            },
        );
        return;
    }

    if !always_suggest_current_token {
        return;
    }

    suggestions.insert(
        0,
        Suggestion {
            name: query.to_string(),
            description: "Enter the current argument".into(),
            kind: "auto-execute".into(),
            args_hint: String::new(),
            insert_value: Some("\n".into()),
            display_name: None,
            primary_name: None,
            separator_to_add: None,
            should_add_space: false,
            hidden: false,
            priority: i64::MAX,
            icon: Some("fig://icon?type=carrot".into()),
            original_type: None,
            query_term: None,
            is_dangerous: false,
            requires_arg: false,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Registry;
    use std::fs;

    fn load_git() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [
                {"names": ["checkout"], "description": "Switch branches"},
                {"names": ["commit"], "description": "Record changes"},
                {"names": ["cherry-pick"], "description": "Apply commits"},
                {"names": ["status"], "description": "Show status"}
              ],
              "options": [{"names": ["--help"], "description": "Show help"}]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn current_command_slice_follows_shell_separators_and_assignments() {
        assert_eq!(current_command_slice("git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && git ch"), "git ch");
        assert_eq!(current_command_slice("echo x&&git ch"), "git ch");
        assert_eq!(current_command_slice("FOO+=1 git ch"), "git ch");
        assert_eq!(current_command_slice("echo x || git ch"), "git ch");
        assert_eq!(current_command_slice("echo x | grep --"), "grep --");
        assert_eq!(current_command_slice("echo x |& grep --"), "grep --");
        assert_eq!(current_command_slice("echo x; git ch"), "git ch");
        assert_eq!(current_command_slice("echo x & git ch"), "git ch");
        assert_eq!(current_command_slice("FOO=1 git ch"), "git ch");
        assert_eq!(current_command_slice("FOO=1 BAR=2 git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && FOO=1 git ch"), "git ch");
        assert_eq!(current_command_slice(r#"echo "x && git" ch"#), r#"echo "x && git" ch"#);
        assert_eq!(current_command_slice("echo $(git ch"), "git ch");
        assert_eq!(current_command_slice("echo `git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && (cd /tmp && git ch"), "git ch");
        assert_eq!(current_command_slice("{ echo x; git ch"), "git ch");
        assert_eq!(current_command_slice("echo foo 2>&1 && git ch"), "git ch");
        assert_eq!(current_command_slice("echo x && 2>&1 git ch"), "git ch");
        assert_eq!(current_command_slice("FOO=1"), "");
        assert_eq!(current_command_slice("echo x && "), "");
        assert_eq!(current_command_slice("'FOO=1' git ch"), "'FOO=1' git ch");
        assert_eq!(current_command_slice(r#""FOO=1" git ch"#), r#""FOO=1" git ch"#);
        assert_eq!(current_command_slice("FOO=1 (git ch"), "git ch");
        assert_eq!(current_command_slice("echo `foo)` && git ch"), "git ch");
        assert!(is_fresh_command_position("echo x && "));
        assert!(!is_fresh_command_position("echo x &&"));
        assert!(!is_fresh_command_position(""));
        assert!(!is_fresh_command_position("FOO=1 "));
        assert!(!is_fresh_command_position("echo x && FOO=1 "));
    }

    #[test]
    fn git_ch_matches_checkout_and_cherry_pick() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert!(names.contains(&"cherry-pick"), "{names:?}");
        assert!(!names.contains(&"status"), "{names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn chained_command_completes_the_current_command_not_the_first() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "echo x && git ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert!(names.contains(&"cherry-pick"), "{names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn chained_quoted_token_keeps_the_raw_search_term() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "echo x && git 'ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert_eq!(result.search_term, "'ch");
    }

    #[test]
    fn assignment_prefix_does_not_hide_the_root_command() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "FOO=1 git ch".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert_eq!(result.search_term, "ch");
    }

    #[test]
    fn fuzzy_matches_non_prefix_subsequence() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ckt".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: true,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"checkout"), "{names:?}");
        assert!(!names.contains(&"commit"), "{names:?}");
    }

    #[test]
    fn trailing_space_lists_all_subcommands() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        assert!(result.suggestions.iter().any(|s| s.name == "status"));
        assert!(result.suggestions.iter().any(|s| s.kind == "option"));
        assert_eq!(result.search_term, "");
    }

    #[test]
    fn first_token_without_space_matches_commands_not_subcommands() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "gi".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"git"), "{names:?}");
        assert!(!names.contains(&"checkout"), "{names:?}");
        assert_eq!(result.search_term, "gi");
    }

    #[test]
    fn separator_with_no_command_uses_first_token_completion() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "echo x && ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"git"), "{names:?}");
        assert_eq!(result.search_term, "");
    }

    #[test]
    fn first_token_can_be_disabled() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "gi".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: false,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        assert!(result.suggestions.is_empty(), "{:?}", result.suggestions);
        assert_eq!(result.search_term, "gi");
    }

    #[test]
    fn disable_for_commands_matches_the_configured_command_array() {
        let disabled = vec!["git".to_string(), "kubectl".to_string()];
        assert!(command_is_disabled_from(&disabled, "git"));
        assert!(!command_is_disabled_from(&disabled, "GIT"));
        assert!(!command_is_disabled_from(&disabled, " kubectl "));
        assert!(!command_is_disabled_from(&disabled, "cargo"));
        assert!(!command_is_disabled_from(&disabled, ""));
        assert!(command_is_disabled_from(&[" kubectl ".to_string()], " kubectl "));
    }

    #[test]
    fn exact_first_token_does_not_list_subcommands() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        assert!(
            result.suggestions.iter().all(|s| s.name != "checkout"),
            "{:?}",
            result.suggestions
        );
        assert_eq!(result.search_term, "git");
    }

    #[test]
    fn cursor_slices_buffer_so_mid_line_completes_the_token_before_caret() {
        let (_dir, mut registry) = load_git();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git checkout".into(),
                cwd: "/".into(),
                cursor: Some(4),
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        assert!(
            result.suggestions.iter().any(|s| s.name == "status"),
            "{:?}",
            result.suggestions
        );
        assert_eq!(result.search_term, "");
    }

    #[test]
    fn args_hint_formats_optional_and_variadic() {
        assert_eq!(
            args_hint(&[ArgSpec {
                name: "path".into(),
                is_optional: true,
                is_variadic: true,
                ..ArgSpec::default()
            }]),
            "[path...]"
        );
        assert_eq!(
            args_hint(&[ArgSpec {
                name: "branch".into(),
                ..ArgSpec::default()
            }]),
            "<branch>"
        );
    }

    fn load_context_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names":["git"],
              "args":[{"name":"root path","description":"Root positional"}],
              "options":[
                {"names":["--message","-m"],"description":"Commit message","args":[{"name":"message","description":"Message value"}]},
                {"names":["--optional"],"args":[{"name":"maybe","description":"Optional value","isOptional":true}]}
              ],
              "subcommands":[
                {"names":["checkout"],"args":[
                  {"name":"branch","description":"Branch value"},
                  {"name":"path","description":"Path value","isOptional":true}
                ]}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    fn load_option_state_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "options":[
                {"names":["--one","-o"],"description":"One","exclusiveOn":["--two","-t"],"dependsOn":["--needed"]},
                {"names":["--two","-t"],"description":"Two"},
                {"names":["--needed"],"description":"Needed"},
                {"names":["--once"],"description":"Once"},
                {"names":["--twice"],"description":"Twice","isRepeatable":2},
                {"names":["--many"],"description":"Many","isRepeatable":true}
              ],
              "persistentOptions":[
                {"names":["--global","-g"],"description":"Global","isPersistent":true}
              ],
              "subcommands":[
                {"names":["child"],"options":[{"names":["--child"],"description":"Child"}]}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    fn load_parser_directive_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("strict.json"),
            r#"{
              "names":["strict"],
              "parserDirectives":{
                "optionsMustPrecedeArguments":true,
                "flagsArePosixNoncompliant":true,
                "optionArgSeparators":[":","="]
              },
              "args":[{"name":"value"}],
              "options":[
                {"names":["--flag"],"description":"Flag"},
                {"names":["-word"],"description":"Non-POSIX long","requiresSeparator":":","args":[{"name":"value"}]},
                {"names":["--required"],"description":"Required separator","requiresSeparator":true,"args":[{"name":"value"}]},
                {"names":["--files"],"description":"Files","args":[{"name":"file","isVariadic":true,"optionsCanBreakVariadicArg":false}]}
              ],
              "subcommands":[
                {"names":["child"],"parserDirectives":{"optionsMustPrecedeArguments":false},"args":[{"name":"values","isVariadic":true,"optionsCanBreakVariadicArg":false}],"options":[{"names":["--child-flag"],"description":"Child flag"}]},
                {"names":["break"],"parserDirectives":{"optionsMustPrecedeArguments":false},"args":[{"name":"values","isVariadic":true,"optionsCanBreakVariadicArg":true}],"options":[{"names":["--child-flag"],"description":"Child flag"}]},
                {"names":["posix"],"parserDirectives":{"flagsArePosixNoncompliant":false},"options":[{"names":["-word"],"args":[{"name":"long-value"}]},{"names":["-w"]},{"names":["-o"]},{"names":["-r"]},{"names":["-d"]}]}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn parser_directives_stop_options_after_positional_arguments() {
        let (_dir, mut registry) = load_parser_directive_spec();
        let result = context_result(&mut registry, "strict value ");
        assert!(!result.suggestions.iter().any(|s| s.name == "--flag"));
    }

    #[test]
    fn parser_directives_control_variadic_option_breaks_and_current_arg() {
        let (_dir, mut registry) = load_parser_directive_spec();
        let result = context_result(&mut registry, "strict --files one ");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("file"));
        assert!(!result.suggestions.iter().any(|s| s.name == "--flag"));

        let result = context_result(&mut registry, "strict break one ");
        assert!(result.suggestions.iter().any(|s| s.name == "--child-flag"));

        // A known option-looking token is still a variadic value when the
        // positional arg explicitly disallows option breaks, both while the
        // token is being edited and after it has been completed.
        let result = context_result(&mut registry, "strict child one --child-flag");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("values"));
        assert!(!result.suggestions.iter().any(|s| s.name == "--child-flag"));
        let result = context_result(&mut registry, "strict child one --child-flag ");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("values"));
        assert!(!result.suggestions.iter().any(|s| s.name == "--child-flag"));

        // POSIX mode must parse a multi-letter single-dash token as a short
        // chain before considering an exact `-word` option.
        let result = context_result(&mut registry, "strict posix -word ");
        assert!(result.current_arg.is_none());
    }

    #[test]
    fn parser_directives_parse_custom_attached_separator_and_non_posix_flags() {
        let (_dir, mut registry) = load_parser_directive_spec();
        for buffer in ["strict -word:abc", "strict -word=abc"] {
            let result = context_result(&mut registry, buffer);
            assert_eq!(result.search_term, "abc", "buffer={buffer}");
            assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("value"));
            assert!(result.suggestions.is_empty());
        }

        let result = context_result(&mut registry, "strict -wo");
        let word = result
            .suggestions
            .iter()
            .find(|s| s.name == "-word")
            .expect("non-POSIX option");
        assert_eq!(word.separator_to_add.as_deref(), Some(":"));
        let result = context_result(&mut registry, "strict --req");
        let required = result
            .suggestions
            .iter()
            .find(|s| s.name == "--required")
            .expect("boolean requiresSeparator option");
        assert_eq!(required.separator_to_add.as_deref(), Some(":"));
    }

    #[test]
    fn filter_strategy_uses_spec_only_without_an_active_argument() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "filterStrategy":"fuzzy",
              "subcommands":[{"names":["checkout"]},{"names":["cherry-pick"]}],
              "additionalSuggestions":[
                {"names":["deploy"],"type":"shortcut"},
                {"names":["bar"],"type":"shortcut","getQueryTerm":"/"}
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "tool ce".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        assert!(result.fuzzy);
        assert!(result.suggestions.iter().any(|item| item.name == "checkout"));

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "tool ?de".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let deploy = result
            .suggestions
            .iter()
            .find(|item| item.name == "deploy")
            .expect("shortcut query strips the leading question mark");
        assert_eq!(deploy.query_term.as_deref(), Some("de"));

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "tool ?foo/bar".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let explicit = result
            .suggestions
            .iter()
            .find(|item| item.name == "bar")
            .expect("explicit getQueryTerm wins over shortcut handling");
        assert_eq!(explicit.query_term.as_deref(), Some("bar"));
    }

    #[test]
    fn active_argument_filter_strategy_does_not_inherit_parent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("argtool.json"),
            r#"{
              "names":["argtool"],
              "filterStrategy":"fuzzy",
              "args":[{"name":"value","suggestions":[{"names":["target"]}]}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: false,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        assert!(!result.fuzzy);
        assert!(!result.suggestions.iter().any(|item| item.name == "target"));

        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "argtool tr".into(),
                fuzzy: true,
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        assert!(result.fuzzy);
        assert!(
            result.suggestions.iter().any(|item| item.name == "target"),
            "fuzzy={} current_arg={:?} suggestions={:?}",
            result.fuzzy,
            result.current_arg.as_ref().map(|arg| &arg.name),
            result.suggestions.iter().map(|item| &item.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn static_named_categories_sort_without_reordering_positional_rows() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("sortme.json"),
            r#"{
              "names":["sortme"],
              "args":[{"name":"value","suggestions":[{"names":["zulu"]},{"names":["alpha"]}]}],
              "subcommands":[{"names":["sub-zulu"]},{"names":["sub-alpha"]}],
              "additionalSuggestions":[{"names":["add-zulu"]},{"names":["add-alpha"]}],
              "options":[{"names":["--zulu"]},{"names":["--alpha"]}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = context_result(&mut registry, "sortme ");
        let names: Vec<_> = result.suggestions.iter().map(|item| item.name.as_str()).collect();
        let sub_start = names.iter().position(|name| *name == "sub-alpha").unwrap();
        assert_eq!(&names[sub_start..sub_start + 2], ["sub-alpha", "sub-zulu"]);
        let positional_start = names
            .iter()
            .position(|name| *name == "zulu")
            .unwrap_or_else(|| panic!("names={names:?}"));
        assert_eq!(&names[positional_start..positional_start + 2], ["zulu", "alpha"]);
        let additional_start = names.iter().position(|name| *name == "add-alpha").unwrap();
        assert_eq!(
            &names[additional_start..additional_start + 2],
            ["add-alpha", "add-zulu"]
        );
        let option_start = names.iter().position(|name| *name == "--alpha").unwrap();
        assert_eq!(&names[option_start..option_start + 2], ["--alpha", "--zulu"]);
    }

    #[test]
    fn option_state_filters_exclusive_repeatable_and_promotes_dependencies() {
        let (_dir, mut registry) = load_option_state_spec();
        let result = context_result(&mut registry, "tool -o ");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"--two"), "{names:?}");
        assert!(!names.contains(&"-t"), "{names:?}");
        assert_eq!(
            result
                .suggestions
                .iter()
                .find(|s| s.name == "--needed")
                .map(|s| s.priority),
            Some(75)
        );

        let result = context_result(&mut registry, "tool --once --once ");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"--once"), "{names:?}");

        let result = context_result(&mut registry, "tool --twice --twice ");
        let names: Vec<_> = result.suggestions.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"--twice"), "{names:?}");

        let result = context_result(&mut registry, "tool --many --many ");
        assert!(result.suggestions.iter().any(|s| s.name == "--many"));
    }

    #[test]
    fn persistent_options_follow_subcommands_and_aliases_count_as_one_option() {
        let (_dir, mut registry) = load_option_state_spec();
        let result = context_result(&mut registry, "tool child ");
        assert!(result.suggestions.iter().any(|s| s.name == "--global"));

        let result = context_result(&mut registry, "tool -o ");
        assert!(result.suggestions.iter().any(|s| s.name == "--needed"));
        let result = context_result(&mut registry, "tool --one ");
        assert!(!result.suggestions.iter().any(|s| s.name == "--one"));
    }

    fn context_result(registry: &mut Registry, buffer: &str) -> CompleteResult {
        complete(
            registry,
            &CompleteRequest {
                buffer: buffer.into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        )
    }

    fn load_option_chain_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "additionalSuggestions":[{"names":["shortcut"],"description":"Shortcut"}],
              "options":[
                {"names":["-a","--all"],"description":"All"},
                {"names":["-b","--brief"],"description":"Brief"},
                {"names":["-m","--message"],"description":"Message","args":[{
                  "name":"message",
                  "description":"Message value",
                  "suggestions":[{"names":["foo"]},{"names":["foobar"]}]
                }]},
                {"names":["--long"],"description":"Long only"}
              ]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    fn load_argument_load_spec() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.json"),
            r#"{
              "names":["tool"],
              "args":[{"name":"input","loadSpec":"positional-target"}],
              "options":[{"names":["--profile","-p"],"args":[{"name":"profile","loadSpec":"profile-target"}]}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("positional-target.json"),
            r#"{
              "names":["positional-target"],
              "args":[{"name":"next input"}],
              "subcommands":[{"names":["child"]}],
              "options":[{"names":["--target-only"]}]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("profile-target.json"),
            r#"{
              "names":["profile-target"],
              "args":[{"name":"next profile"}],
              "subcommands":[{"names":["list"]}],
              "options":[{"names":["--profile-only"]}]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn current_arg_tracks_positional_query_without_consuming_it() {
        let (_dir, mut registry) = load_context_spec();
        let result = context_result(&mut registry, "git checkout fe");
        assert_eq!(result.search_term, "fe");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("branch"));
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.description.as_str()),
            Some("Branch value")
        );
    }

    #[test]
    fn current_arg_advances_after_a_completed_positional_token() {
        let (_dir, mut registry) = load_context_spec();
        let result = context_result(&mut registry, "git checkout feature ");
        assert_eq!(result.search_term, "");
        assert_eq!(result.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("path"));
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.description.as_str()),
            Some("Path value")
        );
    }

    #[test]
    fn completed_positional_load_spec_replaces_context_but_not_the_editing_token() {
        let (_dir, mut registry) = load_argument_load_spec();

        let editing = context_result(&mut registry, "tool value");
        assert_eq!(editing.current_arg.as_ref().map(|arg| arg.name.as_str()), Some("input"));
        assert!(!editing.suggestions.iter().any(|suggestion| suggestion.name == "child"));

        let completed = context_result(&mut registry, "tool value ");
        assert_eq!(
            completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("next input")
        );
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "child")
        );
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "--target-only")
        );
    }

    #[test]
    fn completed_separated_option_value_loads_the_option_spec() {
        let (_dir, mut registry) = load_argument_load_spec();

        let editing = context_result(&mut registry, "tool --profile value");
        assert_eq!(
            editing.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("profile")
        );
        assert!(!editing.suggestions.iter().any(|suggestion| suggestion.name == "list"));

        let completed = context_result(&mut registry, "tool --profile value ");
        assert_eq!(
            completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("next profile")
        );
        assert!(completed.suggestions.iter().any(|suggestion| suggestion.name == "list"));
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "--profile-only")
        );
    }

    #[test]
    fn completed_equals_and_short_attached_values_load_the_option_spec() {
        let (_dir, mut registry) = load_argument_load_spec();

        for buffer in ["tool --profile=value ", "tool -pvalue "] {
            let completed = context_result(&mut registry, buffer);
            assert_eq!(
                completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
                Some("next profile"),
                "buffer={buffer}"
            );
            assert!(
                completed.suggestions.iter().any(|suggestion| suggestion.name == "list"),
                "buffer={buffer}, suggestions={:?}",
                completed.suggestions
            );
        }

        for buffer in ["tool --profile=value", "tool -pvalue"] {
            let editing = context_result(&mut registry, buffer);
            assert_eq!(
                editing.current_arg.as_ref().map(|arg| arg.name.as_str()),
                Some("profile"),
                "buffer={buffer}"
            );
            assert!(
                editing.suggestions.iter().all(|suggestion| suggestion.name != "list"),
                "buffer={buffer}, suggestions={:?}",
                editing.suggestions
            );
        }
    }

    #[test]
    fn positional_load_spec_after_double_dash_still_switches_context() {
        let (_dir, mut registry) = load_argument_load_spec();
        let completed = context_result(&mut registry, "tool -- value ");
        assert_eq!(
            completed.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("next input")
        );
        assert!(
            completed
                .suggestions
                .iter()
                .any(|suggestion| suggestion.name == "child")
        );
        assert!(
            completed
                .suggestions
                .iter()
                .all(|suggestion| suggestion.name != "--profile-only")
        );
    }

    #[test]
    fn current_arg_handles_option_value_and_equals_form() {
        let (_dir, mut registry) = load_context_spec();
        for (buffer, search) in [
            ("git --message foo", "foo"),
            ("git --message ", ""),
            ("git --message=foo", "foo"),
        ] {
            let result = context_result(&mut registry, buffer);
            assert_eq!(result.search_term, search, "buffer={buffer}");
            assert_eq!(
                result.current_arg.as_ref().map(|arg| arg.name.as_str()),
                Some("message"),
                "buffer={buffer}"
            );
            assert!(
                result.suggestions.is_empty(),
                "an option value must not fall back to sibling options: buffer={buffer}, suggestions={:?}",
                result.suggestions
            );
        }
        assert!(context_result(&mut registry, "git --message").current_arg.is_none());
    }

    #[test]
    fn short_option_chain_extends_each_short_option_and_keeps_the_current_one() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -ab");
        let option_names = result
            .suggestions
            .iter()
            .filter(|suggestion| suggestion.kind == "option")
            .map(|suggestion| suggestion.name.as_str())
            .collect::<Vec<_>>();
        assert!(option_names.contains(&"-ab"), "{option_names:?}");
        assert!(option_names.contains(&"-aba"), "{option_names:?}");
        assert!(option_names.contains(&"-abm"), "{option_names:?}");
        assert!(!option_names.contains(&"--long"), "{option_names:?}");
        assert!(
            result
                .suggestions
                .iter()
                .find(|suggestion| suggestion.name == "shortcut")
                .is_some_and(|suggestion| suggestion.query_term.as_deref() == Some(""))
        );
    }

    #[test]
    fn mandatory_last_chain_option_shows_its_args_without_deleting_the_chain() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -am");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("message")
        );
        assert!(
            result
                .suggestions
                .iter()
                .any(|suggestion| { suggestion.name == "foo" && suggestion.query_term.as_deref() == Some("") })
        );
        assert!(result.suggestions.iter().any(|suggestion| {
            suggestion.kind == "option" && suggestion.name == "-am" && suggestion.query_term.as_deref() == Some("-am")
        }));
        assert!(
            result
                .suggestions
                .iter()
                .all(|suggestion| suggestion.name != "-ama" && suggestion.name != "-amb")
        );
    }

    #[test]
    fn attached_short_chain_argument_replaces_only_its_value() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -amfoo");
        assert_eq!(result.search_term, "foo");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("message")
        );
        assert!(result.suggestions.iter().any(|suggestion| suggestion.name == "foobar"));
        assert!(result.suggestions.iter().all(|suggestion| suggestion.kind != "option"));
    }

    #[test]
    fn completed_short_chain_is_consumed_before_the_argument_context() {
        let (_dir, mut registry) = load_option_chain_spec();
        let result = context_result(&mut registry, "tool -am ");
        assert_eq!(result.search_term, "");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("message")
        );
        assert!(result.suggestions.iter().any(|suggestion| suggestion.name == "foo"));
        assert!(result.suggestions.iter().all(|suggestion| suggestion.kind != "option"));
    }

    #[test]
    fn unknown_chain_and_long_options_keep_the_normal_path() {
        let (_dir, mut registry) = load_option_chain_spec();
        let unknown = context_result(&mut registry, "tool -az");
        assert!(unknown.suggestions.iter().all(|suggestion| suggestion.kind != "option"));

        let long = context_result(&mut registry, "tool --long");
        assert!(
            long.suggestions
                .iter()
                .any(|suggestion| suggestion.kind == "option" && suggestion.name == "--long")
        );
    }

    #[test]
    fn current_arg_honors_double_dash_and_shell_quoting() {
        let (_dir, mut registry) = load_context_spec();
        let result = context_result(&mut registry, "git -- fe\\ bar");
        assert_eq!(result.search_term, "fe\\ bar");
        assert_eq!(
            result.current_arg.as_ref().map(|arg| arg.name.as_str()),
            Some("root path")
        );
        let (tokens, trailing) = tokenize("git 'fe bar' ");
        assert_eq!(tokens, vec!["git", "fe bar"]);
        assert!(trailing);
    }

    #[test]
    fn subcommand_args_hint_comes_from_ir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [
                {
                  "names": ["checkout"],
                  "description": "Switch branches",
                  "args": [{"name": "branch", "isOptional": true}]
                }
              ]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                cwd: "/".into(),
                cursor: None,
                fuzzy: false,
                history_only: false,
                include_history: true,
                suggest_first_token: true,
                current_shell: None,
                current_process: None,
                environment_variables: Default::default(),
            },
        );
        let checkout = result
            .suggestions
            .iter()
            .find(|s| s.name == "checkout")
            .expect("checkout");
        assert_eq!(checkout.args_hint, "[branch]");
    }

    #[test]
    fn subcommand_with_children_adds_a_trailing_space_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["remote"],"subcommands":[{"names":["-v"]}]}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                ..CompleteRequest::default()
            },
        );
        let remote = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "remote")
            .expect("remote");
        assert!(remote.should_add_space);
    }

    #[test]
    fn additional_suggestions_are_available_after_a_trailing_space() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.json"),
            r#"{
              "names": ["git"],
              "subcommands": [{"names": ["commit"]}],
              "additionalSuggestions": [{"names": ["commit -m 'msg'"], "description": "Git commit shortcut"}]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "git ".into(),
                cwd: "/".into(),
                ..CompleteRequest::default()
            },
        );
        let shortcut = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "commit -m 'msg'")
            .expect("additional suggestion");
        assert_eq!(shortcut.kind, "arg");
        assert_eq!(shortcut.description, "Git commit shortcut");
        assert_eq!(shortcut.icon.as_deref(), Some("fig://template?color=628dad&badge=➡️"));
    }

    #[test]
    fn static_suggestion_type_query_term_and_original_type_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.json"),
            r#"{
              "names": ["demo"],
              "args": [{
                "name": "target",
                "suggestions": [{
                  "names": ["file.txt"],
                  "description": "A file",
                  "type": "file",
                  "originalType": "folder",
                  "getQueryTerm": "/"
                }]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "demo dir/fi".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let suggestion = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "file.txt")
            .expect("static suggestion");
        assert_eq!(suggestion.kind, "file");
        assert_eq!(suggestion.original_type.as_deref(), Some("folder"));
        assert_eq!(suggestion.query_term.as_deref(), Some("fi"));
        // Keep the raw shell token for deletion; the per-row query term tells
        // insertion to delete only the suffix after the slash.
        assert_eq!(result.search_term, "dir/fi");
        assert_eq!(result.match_term, "dir/fi");
    }

    #[test]
    fn argument_query_term_changes_match_term_for_static_rows() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.json"),
            r#"{
              "names": ["demo"],
              "args": [{
                "name": "target",
                "getQueryTerm": "/",
                "suggestions": [{"names": ["file.txt"], "type": "file"}]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = complete(
            &mut registry,
            &CompleteRequest {
                buffer: "demo dir/fi".into(),
                include_history: false,
                ..CompleteRequest::default()
            },
        );
        let suggestion = result
            .suggestions
            .iter()
            .find(|suggestion| suggestion.name == "file.txt")
            .expect("static suggestion");
        assert_eq!(suggestion.query_term.as_deref(), Some("fi"));
        assert_eq!(result.search_term, "dir/fi");
        assert_eq!(result.match_term, "fi");
    }

    #[test]
    fn exact_safe_item_gets_an_auto_execute_row_when_enabled() {
        let mut suggestions = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_exact_auto_execute(&mut suggestions, "status", false, false, false);
        assert_eq!(suggestions[0].kind, "auto-execute");
        assert_eq!(suggestions[0].insert_value.as_deref(), Some("\n"));
        assert_eq!(suggestions[0].original_type.as_deref(), Some("subcommand"));
        assert_eq!(suggestions[1].name, "status");
    }

    #[test]
    fn display_name_alone_does_not_create_an_exact_auto_execute_row() {
        let mut suggestion = Suggestion::new("actual", "", "arg");
        suggestion.display_name = Some("Pretty Label".into());
        let mut suggestions = vec![suggestion];
        add_exact_auto_execute(&mut suggestions, "Pretty Label", false, false, false);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].kind, "arg");
    }

    #[test]
    fn exact_alias_uses_the_source_primary_name_for_auto_execute() {
        // The WebView checks names[0] against insertValue, not the alias that
        // happened to match the query.  This is common for canonical names
        // with a short alias (e.g. `checkout` / `co`).
        let suggestion = Suggestion::new("co", "Checkout", "arg")
            .with_insert_value("checkout")
            .with_primary_name(Some("checkout".into()));
        let mut suggestions = vec![suggestion];
        add_exact_auto_execute(&mut suggestions, "co", false, false, false);
        assert_eq!(suggestions[0].kind, "auto-execute");
        assert_eq!(suggestions[1].name, "co");
    }

    #[test]
    fn verbose_alias_selection_uses_the_longest_matching_name() {
        let names = vec!["-h".into(), "--help".into()];
        assert_eq!(
            select_named_candidate(&names, None, "-", false, false).map(String::as_str),
            Some("-h")
        );
        assert_eq!(
            select_named_candidate(&names, None, "-", false, true).map(String::as_str),
            Some("--help")
        );
    }

    #[test]
    fn hidden_names_are_revealed_only_by_an_exact_alias() {
        let names = vec!["secret".into(), "hidden-alias".into()];
        assert!(!hidden_item_is_visible(true, &names, "sec"));
        assert!(!hidden_item_is_visible(true, &names, "Friendly"));
        assert!(hidden_item_is_visible(true, &names, "SECRET"));
        assert!(hidden_item_is_visible(true, &names, "hidden-alias"));
        assert!(hidden_item_is_visible(false, &names, "sec"));
    }

    #[test]
    fn hidden_static_positional_rows_reappear_only_for_case_insensitive_exact_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.json"),
            r#"{
              "names": ["demo"],
              "args": [{
                "name": "value",
                "suggestCurrentToken": true,
                "suggestions": [
                  {"names": ["SecretValue"], "description": "Hidden value", "hidden": true},
                  {"names": ["visible"], "description": "Visible value"}
                ]
              }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();

        let root = registry.get_arc("demo").expect("demo spec");
        let (tokens, trailing) = tokenize("demo vis");
        let context = resolve_context(root, &tokens, trailing, "vis", "vis", None);
        assert_eq!(
            context
                .active_arg
                .as_ref()
                .and_then(|active| active.arg.suggest_current_token),
            Some(true)
        );

        let exact = context_result(&mut registry, "demo secretvalue");
        assert!(
            exact
                .suggestions
                .iter()
                .any(|suggestion| { suggestion.name == "SecretValue" && suggestion.description == "Hidden value" })
        );

        let partial = context_result(&mut registry, "demo secret");
        assert!(partial.suggestions.is_empty(), "{:?}", partial.suggestions);

        let empty = context_result(&mut registry, "demo ");
        assert!(
            empty
                .suggestions
                .iter()
                .all(|suggestion| suggestion.name != "SecretValue")
        );
    }

    #[test]
    fn argument_suggest_current_token_override_wins_over_global_setting() {
        let disabled = ArgSpec {
            suggest_current_token: Some(false),
            ..ArgSpec::default()
        };
        let enabled = ArgSpec {
            suggest_current_token: Some(true),
            ..ArgSpec::default()
        };
        let inherited = ArgSpec::default();
        fn active(arg: &ArgSpec) -> ActiveArg {
            ActiveArg {
                arg: arg.clone(),
                query: "value".into(),
                search_term: "value".into(),
                exclusive: false,
            }
        }

        let disabled_active = active(&disabled);
        let enabled_active = active(&enabled);
        let inherited_active = active(&inherited);
        assert!(!suggest_current_token_for(Some(&disabled_active), true));
        assert!(suggest_current_token_for(Some(&enabled_active), false));
        assert!(suggest_current_token_for(Some(&inherited_active), true));
        assert!(!suggest_current_token_for(Some(&inherited_active), false));
        assert!(suggest_current_token_for(None, true));
    }

    #[test]
    fn exact_item_with_required_argument_does_not_auto_execute() {
        let mut required = Suggestion::new("commit", "Commit", "subcommand");
        required.requires_arg = true;
        // Mandatory unnamed args have no visible hint, but must still prevent
        // Enter from executing an incomplete command.
        assert!(required.args_hint.is_empty());
        let mut suggestions = vec![required];
        add_exact_auto_execute(&mut suggestions, "commit", false, false, false);
        assert_eq!(suggestions.len(), 1);
    }

    #[test]
    fn dangerous_exact_items_need_the_explicit_run_setting() {
        let mut suggestions = vec![Suggestion::new("clean", "Remove files", "subcommand").with_dangerous(true)];
        add_exact_auto_execute(&mut suggestions, "clean", false, false, false);
        assert_eq!(suggestions.len(), 1);
        add_exact_auto_execute(&mut suggestions, "clean", false, false, true);
        assert_eq!(suggestions[0].kind, "auto-execute");
    }

    #[test]
    fn partial_dangerous_match_still_allows_current_token_fallback() {
        let mut suggestions = vec![Suggestion::new("clean", "Remove files", "subcommand").with_dangerous(true)];
        add_current_token_auto_execute(&mut suggestions, "cl", true, false, false, false);
        assert_eq!(suggestions[0].kind, "auto-execute");
        assert_eq!(suggestions[0].name, "cl");
    }

    #[test]
    fn exact_dangerous_match_blocks_current_token_fallback() {
        let mut suggestions = vec![Suggestion::new("clean", "Remove files", "subcommand").with_dangerous(true)];
        add_current_token_auto_execute(&mut suggestions, "clean", true, false, false, false);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].kind, "subcommand");
    }

    #[test]
    fn directory_auto_execute_special_cases_do_not_require_current_token_setting() {
        let mut dot = vec![Suggestion::new(".gitignore", "File", "file")];
        add_current_token_auto_execute(&mut dot, ".", false, false, false, false);
        assert_eq!(dot[0].name, ".");
        assert_eq!(dot[0].description, "Enter the current directory");
        assert_eq!(dot[0].original_type.as_deref(), Some("folder"));

        let mut folder = vec![Suggestion::new("src/", "Folder", "folder")];
        add_current_token_auto_execute(&mut folder, "src/", false, false, false, false);
        assert_eq!(folder[0].name, "↪");
        assert_eq!(folder[0].description, "Enter the current directory");
    }

    #[test]
    fn auto_execute_rows_still_obey_visibility_configuration() {
        let mut exact_hidden = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_exact_auto_execute(&mut exact_hidden, "status", true, false, false);
        assert!(exact_hidden.iter().all(|item| item.kind != "auto-execute"));

        let mut dot_hidden = vec![Suggestion::new(".gitignore", "File", "file")];
        add_current_token_auto_execute(&mut dot_hidden, ".", false, true, false, false);
        assert!(dot_hidden.iter().all(|item| item.kind != "auto-execute"));

        let mut exact_tab_only = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_exact_auto_execute(&mut exact_tab_only, "status", false, true, false);
        assert!(exact_tab_only.iter().all(|item| item.kind != "auto-execute"));
    }

    #[test]
    fn exact_folder_match_uses_the_directory_action_name() {
        let mut suggestions = vec![Suggestion::new("src/", "Folder", "folder")];
        add_exact_auto_execute(&mut suggestions, "src", false, false, false);
        assert_eq!(suggestions[0].name, "src");
        assert_eq!(suggestions[0].description, "folder");
        assert_eq!(suggestions[0].original_type.as_deref(), Some("folder"));
    }

    #[test]
    fn trailing_space_can_add_immediate_execute_row() {
        let mut suggestions = vec![Suggestion::new("status", "Show status", "subcommand")];
        add_space_auto_execute(&mut suggestions, "", true, false, false);
        assert_eq!(suggestions[0].name, "↪");
        assert_eq!(suggestions[0].insert_value.as_deref(), Some("\n"));
    }

    #[test]
    fn always_suggest_current_token_adds_an_action_row() {
        let mut suggestions = vec![Suggestion::new("checkout", "", "subcommand")];
        add_current_token_auto_execute(&mut suggestions, "ch", true, false, false, true);
        assert_eq!(suggestions[0].name, "ch");
        assert_eq!(suggestions[0].description, "Enter the current argument");
        assert_eq!(suggestions[0].insert_value.as_deref(), Some("\n"));
    }

    fn load_is_command_registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("wrapper.json"),
            r#"{
              "names": ["wrapper"],
              "args": [{ "name": "command", "isCommand": true }],
              "options": [{
                "names": ["-u", "--user"],
                "args": [{ "name": "user" }]
              }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("target.json"),
            r#"{
              "names": ["target"],
              "subcommands": [
                { "names": ["build"], "description": "Build the project" },
                { "names": ["test"], "description": "Run tests" }
              ]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("eslint.json"),
            r#"{
              "names": ["eslint"],
              "options": [{ "names": ["--fix"], "description": "Fix problems" }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("modhost.json"),
            r#"{
              "names": ["modhost"],
              "args": [{ "name": "module", "isModule": "lang/" }]
            }"#,
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("lang")).unwrap();
        fs::write(
            dir.path().join("lang/http.json"),
            r#"{
              "names": ["lang/http"],
              "subcommands": [{ "names": ["get"], "description": "GET" }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("dockerish.json"),
            r#"{
              "names": ["dockerish"],
              "subcommands": [{
                "names": ["exec"],
                "args": [
                  { "name": "container" },
                  { "name": "command", "isCommand": true }
                ]
              }]
            }"#,
        )
        .unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        (dir, registry)
    }

    #[test]
    fn is_command_token_lists_bundled_commands() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "wrapper ta");
        assert!(
            result.suggestions.iter().any(|item| item.name == "target"),
            "{:?}",
            result.suggestions
        );
        let empty = context_result(&mut registry, "wrapper ");
        assert!(
            empty.suggestions.iter().any(|item| item.name == "target"),
            "{:?}",
            empty.suggestions
        );
    }

    #[test]
    fn is_command_switches_to_the_named_spec() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "wrapper target b");
        assert!(
            result.suggestions.iter().any(|item| item.name == "build"),
            "{:?}",
            result.suggestions
        );
        let after_space = context_result(&mut registry, "wrapper target ");
        assert!(
            after_space.suggestions.iter().any(|item| item.name == "test"),
            "{:?}",
            after_space.suggestions
        );
    }

    #[test]
    fn is_command_follows_an_option_value() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "wrapper -u root target te");
        assert!(
            result.suggestions.iter().any(|item| item.name == "test"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn nested_is_command_keeps_switching() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("sudoish.json"),
            r#"{
              "names": ["sudoish"],
              "args": [{ "name": "command", "isCommand": true }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = context_result(&mut registry, "sudoish sudoish sudoish ");
        assert!(
            result.suggestions.iter().any(|item| item.name == "sudoish"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn is_command_on_a_later_positional_arg() {
        let (_dir, mut registry) = load_is_command_registry();
        let result = context_result(&mut registry, "dockerish exec box eslint --");
        assert!(
            result.suggestions.iter().any(|item| item.name == "--fix"),
            "{:?}",
            result.suggestions
        );
    }

    #[test]
    fn is_module_loads_the_prefixed_spec() {
        let (_dir, mut registry) = load_is_command_registry();
        let typing = context_result(&mut registry, "modhost ht");
        assert!(
            typing.suggestions.iter().any(|item| item.name == "http"),
            "{:?}",
            typing.suggestions
        );
        let switched = context_result(&mut registry, "modhost http g");
        assert!(
            switched.suggestions.iter().any(|item| item.name == "get"),
            "{:?}",
            switched.suggestions
        );
    }

    #[test]
    fn load_spec_wins_over_is_command() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("host.json"),
            r#"{
              "names": ["host"],
              "args": [{
                "name": "command",
                "isCommand": true,
                "loadSpec": {
                  "names": ["forced"],
                  "subcommands": [{ "names": ["only"], "description": "From loadSpec" }]
                }
              }]
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("other.json"),
            r#"{
              "names": ["other"],
              "subcommands": [{ "names": ["nope"], "description": "Should not load" }]
            }"#,
        )
        .unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let result = context_result(&mut registry, "host other ");
        assert!(
            result.suggestions.iter().any(|item| item.name == "only"),
            "{:?}",
            result.suggestions
        );
        assert!(result.suggestions.iter().all(|item| item.name != "nope"));
    }

    #[test]
    fn command_lookup_name_uses_basename_for_paths() {
        assert_eq!(command_lookup_name("git", false), "git");
        assert_eq!(command_lookup_name("foo/bar", false), "foo/bar");
        assert_eq!(command_lookup_name("./bin/git", false), "git");
        assert_eq!(command_lookup_name("/usr/bin/git", false), "git");
        assert_eq!(command_lookup_name("~/bin/git", true), "git");
    }
}
