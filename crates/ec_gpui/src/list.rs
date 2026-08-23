use std::borrow::Cow;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, BoxShadow, Context, Entity, FontWeight, Image, InteractiveElement, IntoElement, ParentElement, Render,
    Rgba, ScrollStrategy, StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, div, hsla, point, px,
    rgb, uniform_list,
};

use crate::icons::{history_icon_image_element, identifier_icon_element, named_icon_element, png_icon_element};
use crate::overlay::OverlayState;

pub const DEFAULT_FONT_SIZE: f32 = 12.8;
pub const DEFAULT_ROW_HEIGHT: f32 = 20.0;
pub const DESCRIPTION_HEIGHT: f32 = 20.0;
pub const ICON_SIZE: f32 = 15.0;
pub const DEFAULT_WIDTH: f64 = 320.0;
pub const DEFAULT_MAX_LIST_HEIGHT: f64 = 140.0;
pub const POPOUT_WIDTH: f32 = 200.0;
/// The legacy overlay set `:root { font-size: var(--font-size, 12.8px) }`, so
/// one CSS rem was the suggestion font size, not the usual 16px. Every Tailwind
/// spacing on the overlay therefore came out 0.8x the value a 16px basis would
/// suggest, and shrank or grew with `autocomplete.fontSize`.
fn rem(font_size: f32, rems: f32) -> f32 {
    font_size * rems
}

/// Transparent inset around the card, legacy `p-1`. Also a positioning
/// constant: the caret offset is applied to the window, so a larger pad
/// visibly pushes the card away from the caret.
pub fn layout_pad(font_size: f32) -> f32 {
    rem(font_size, 0.25)
}

/// Gap between the card and the description popout, legacy `gap-1.5`.
pub fn layout_gap(font_size: f32) -> f32 {
    rem(font_size, 0.375)
}

/// Card corner, legacy `rounded`.
fn card_radius(font_size: f32) -> f32 {
    rem(font_size, 0.25)
}

/// Suggestion row inset, legacy `pl-1.5`. The legacy row had no matching
/// right pad, so the title runs all the way to the card edge before clipping.
fn row_pad_left(font_size: f32) -> f32 {
    rem(font_size, 0.375)
}
pub const DEV_BANNER_HEIGHT: f32 = 88.0;
/// Static loading marker. Keeping this non-animated avoids a perpetual GPUI
/// invalidation loop if the completion worker or IPC stream gets stuck.
pub const LOADING_DOTS: &str = "···";
/// The legacy cards had no layout border; their shadow fits inside the outer
/// transparent padding.
pub const CARD_BORDER: f32 = 0.0;
/// `#description.popout` used a literal `border-radius: 4px`, so unlike the
/// suggestion card it did not scale with the font size.
const POPOUT_RADIUS: f32 = 4.0;
/// WebView debug mode painted `:root` red so overlay bounds were visible
/// through the transparent pad. Native overlay paints the window the same way.
pub const DEBUG_WINDOW_FILL: u32 = 0xff_00_00;
/// CSS `box-shadow` blur maps to twice the gaussian sigma, and GPUI's
/// `blur_radius` *is* that sigma. The legacy `0 0 3px` therefore needs 1.5
/// here; passing 3.0 doubles the halo and reads as a soft grey smear.
const LEGACY_SHADOW_SIGMA: f32 = 1.5;

fn legacy_card_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: hsla(0.0, 0.0, 85.0 / 255.0, 1.0),
        offset: point(px(0.0), px(0.0)),
        blur_radius: px(LEGACY_SHADOW_SIGMA),
        spread_radius: px(0.0),
    }]
}

/// Keep the icon box at 75% of the row height, as in the old WebView.
pub fn icon_size_for_row(row_height: f32) -> f32 {
    row_height * (ICON_SIZE / DEFAULT_ROW_HEIGHT)
}

#[derive(Clone, Default)]
pub struct SuggestionItem {
    pub name: String,
    pub description: String,
    pub kind: String,
    pub args_hint: String,
    pub insert_value: Option<String>,
    pub display_name: Option<String>,
    /// Canonical first name from the source suggestion. The legacy recency
    /// index records `name[0]` even when another alias is displayed.
    pub primary_name: Option<String>,
    pub separator_to_add: Option<String>,
    pub should_add_space: bool,
    pub hidden: bool,
    pub priority: i64,
    pub icon_identifier: Option<String>,
    pub original_type: Option<String>,
    /// Per-row query after applying a static `getQueryTerm` rule. This may
    /// differ between generator rows in the same result set.
    pub query_term: Option<String>,
    pub icon_png: Option<Arc<Image>>,
}

/// Match the WebView's `updateSuggestions` identity exactly. Presentation-only
/// fields such as `displayName` and a generator's current query term may
/// change between refreshes without forcing the user's selection back to row
/// zero.
pub type SelectionIdentity = (String, String, Option<String>, String);

pub fn selection_identity(item: &SuggestionItem) -> SelectionIdentity {
    (
        item.name.clone(),
        item.kind.clone(),
        item.insert_value.clone(),
        item.description.clone(),
    )
}

pub fn matches_selection_identity(item: &SuggestionItem, identity: &SelectionIdentity) -> bool {
    item.name == identity.0
        && item.kind == identity.1
        && item.insert_value == identity.2
        && item.description == identity.3
}

#[derive(Clone, Debug, Default)]
pub struct ClickInsert {
    pub name: String,
    pub description: String,
    pub search: String,
    pub kind: String,
    pub args_hint: String,
    pub insert_value: Option<String>,
    pub display_name: Option<String>,
    pub primary_name: Option<String>,
    pub separator_to_add: Option<String>,
    pub should_add_space: bool,
    pub hidden: bool,
    pub priority: i64,
    pub icon_identifier: Option<String>,
    pub original_type: Option<String>,
    pub query_term: Option<String>,
}

impl std::fmt::Debug for SuggestionItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuggestionItem")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("kind", &self.kind)
            .field("args_hint", &self.args_hint)
            .field("insert_value", &self.insert_value)
            .field("display_name", &self.display_name)
            .field("primary_name", &self.primary_name)
            .field("separator_to_add", &self.separator_to_add)
            .field("should_add_space", &self.should_add_space)
            .field("hidden", &self.hidden)
            .field("priority", &self.priority)
            .field("icon_identifier", &self.icon_identifier)
            .field("original_type", &self.original_type)
            .field("query_term", &self.query_term)
            .field("icon_png", &self.icon_png.is_some())
            .finish()
    }
}

/// Colors match the WebView overlay's `src/index.css`, last present in
/// `packages/autocomplete-app` at v2.2.2.
#[derive(Clone, Copy)]
pub struct OverlayTheme {
    pub background: u32,
    pub border: u32,
    pub text: u32,
    pub muted: u32,
    pub selected: u32,
    pub selected_text: u32,
    pub match_bg: u32,
    pub selected_match_bg: u32,
    pub accent: u32,
}

impl OverlayTheme {
    pub fn dark() -> Self {
        Self {
            background: 0x303030,
            border: 0x414141,
            text: 0xb4b4b4,
            muted: 0xb4b4b4,
            selected: 0x1e5ac7,
            selected_text: 0xfdfdfd,
            match_bg: 0x5f5938,
            selected_match_bg: 0x6a8eda,
            accent: 0x1e5ac7,
        }
    }

    pub fn light() -> Self {
        Self {
            background: 0xfefefe,
            border: 0xc7c7c7,
            text: 0x070707,
            muted: 0x070707,
            selected: 0x2969da,
            selected_text: 0xfdfdfd,
            match_bg: 0xffef98,
            selected_match_bg: 0x6a8eda,
            accent: 0x2969da,
        }
    }
}

impl Default for OverlayTheme {
    fn default() -> Self {
        Self::dark()
    }
}

pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        "subcommand" => "sub",
        "option" => "opt",
        "folder" | "dir" => "dir",
        "file" => "file",
        "arg" => "arg",
        "history" => "hist",
        _ => "cmd",
    }
}

/// Byte length of the case-insensitive prefix shared by `name` and `search`.
pub fn match_prefix_bytes(name: &str, search: &str) -> usize {
    if search.is_empty() {
        return 0;
    }
    let mut bytes = 0;
    let mut name_chars = name.chars();
    for needle in search.chars() {
        match name_chars.next() {
            Some(haystack) if haystack.eq_ignore_ascii_case(&needle) => bytes += haystack.len_utf8(),
            _ => return 0,
        }
    }
    bytes
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Text,
    Match,
    Prefix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRun {
    pub text: String,
    pub kind: RunKind,
}

pub fn fuzzy_indexes(name: &str, query: &str) -> Option<Vec<usize>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let chars: Vec<char> = name.chars().collect();
    let lower: Vec<char> = chars.iter().map(|ch| ch.to_ascii_lowercase()).collect();
    let search: Vec<char> = query.chars().map(|ch| ch.to_ascii_lowercase()).collect();
    let mut indexes = Vec::with_capacity(search.len());
    let mut pos = 0;
    for needle in &search {
        let found = lower
            .iter()
            .enumerate()
            .skip(pos)
            .find(|(_, haystack)| **haystack == *needle)
            .map(|(index, _)| index)?;
        indexes.push(found);
        pos = found + 1;
    }
    // Fuzzysort first finds a simple subsequence, then prefers a strict match
    // that starts at a word boundary and keeps characters consecutive. This
    // matters for repeated letters (`fooBar`/`fb`, `checkout`/`ckt`) because
    // the highlighted indexes are user-visible, not just a ranking detail.
    let beginnings = beginning_indexes(&chars);
    let next_beginning = next_beginning_indexes(chars.len(), &beginnings);
    let mut strict = vec![0usize; search.len()];
    let mut search_i = 0usize;
    let first = *indexes.first()?;
    let mut target_i = if first == 0 {
        0
    } else {
        next_beginning.get(first - 1).copied().unwrap_or(chars.len())
    };
    let mut success = false;
    loop {
        if target_i >= chars.len() {
            if search_i == 0 {
                break;
            }
            // The strict path ran out of target text. Force the previous
            // match forward to the next word boundary and try again, exactly
            // like fuzzysort's small backtracking pass.
            search_i -= 1;
            let last_match = strict[search_i];
            target_i = next_beginning.get(last_match).copied().unwrap_or(chars.len());
        } else if search[search_i] == lower[target_i] {
            strict[search_i] = target_i;
            search_i += 1;
            if search_i == search.len() {
                success = true;
                break;
            }
            target_i += 1;
        } else {
            let next = next_beginning.get(target_i).copied().unwrap_or(chars.len());
            if next >= chars.len() {
                target_i = chars.len();
            } else {
                target_i = next;
            }
        }
    }
    if success { Some(strict) } else { Some(indexes) }
}

fn beginning_indexes(chars: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut was_upper = false;
    let mut was_alphanumeric = false;
    for (index, ch) in chars.iter().copied().enumerate() {
        let is_upper = ch.is_ascii_uppercase();
        let is_alphanumeric = ch.is_ascii_alphanumeric();
        if (is_upper && !was_upper) || !was_alphanumeric || !is_alphanumeric {
            out.push(index);
        }
        was_upper = is_upper;
        was_alphanumeric = is_alphanumeric;
    }
    out
}

fn next_beginning_indexes(len: usize, beginnings: &[usize]) -> Vec<usize> {
    let mut out = vec![len; len];
    if beginnings.is_empty() {
        return out;
    }
    let mut beginning_i = 0usize;
    let mut next = beginnings[0];
    for (index, slot) in out.iter_mut().enumerate() {
        if next <= index {
            beginning_i += 1;
            next = beginnings.get(beginning_i).copied().unwrap_or(len);
        }
        *slot = next;
    }
    out
}

pub fn longest_common_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix = (*first).to_string();
    for name in names.iter().skip(1) {
        while !name.starts_with(&prefix) {
            prefix.pop();
            if prefix.is_empty() {
                return prefix;
            }
        }
    }
    prefix
}

/// React-window's `scrollToItem(index, "smart")` keeps an item in place when
/// it is already fully visible, and otherwise moves only the nearest edge into
/// view. GPUI's uniform-list API exposes Top/Center/Bottom strategies but no
/// Smart strategy; choose the matching edge based on the current visible
/// range, leaving a visible selection completely untouched.
pub fn smart_scroll_strategy(selected: usize, first_visible: usize, last_visible: usize) -> Option<ScrollStrategy> {
    if selected < first_visible {
        Some(ScrollStrategy::Top)
    } else if selected > last_visible {
        Some(ScrollStrategy::Bottom)
    } else {
        None
    }
}

fn normalize_kind(kind: &str) -> &str {
    match kind {
        "dir" => "folder",
        "cmd" => "subcommand",
        other => other,
    }
}

fn items_match_for_prefix(candidate: &SuggestionItem, selected: &SuggestionItem) -> bool {
    items_match_for_prefix_parts(candidate, &selected.kind, &selected.name)
}

fn items_match_for_prefix_parts(candidate: &SuggestionItem, selected_kind: &str, selected_name: &str) -> bool {
    // Keep the old `isMatchingType` asymmetry: a `../` candidate only joins
    // the prefix set when `../` itself is selected. Without this guard the
    // parent-directory row destroys the useful common prefix of ordinary
    // files/folders (for example `src/` and `scripts/`).
    if candidate.name == "../" {
        return selected_name == "../";
    }
    let a = normalize_kind(&candidate.kind);
    let b = normalize_kind(selected_kind);
    a == b || (is_fileish(a) && is_fileish(b))
}

fn prefix_type_filter(selected: &SuggestionItem) -> (Cow<'_, str>, Cow<'_, str>) {
    if selected.kind == "auto-execute" {
        let kind = selected.original_type.as_deref().unwrap_or("");
        if kind == "folder" && !selected.name.ends_with('/') {
            return (Cow::Borrowed(kind), Cow::Owned(format!("{}/", selected.name)));
        }
        return (Cow::Borrowed(kind), Cow::Borrowed(selected.name.as_str()));
    }
    (
        Cow::Borrowed(selected.kind.as_str()),
        Cow::Borrowed(selected.name.as_str()),
    )
}

fn is_fileish(kind: &str) -> bool {
    matches!(kind, "file" | "folder")
}

/// Shared prefix of same-kind rows, matching the old overlay's Tab underline.
pub fn common_prefix_for(selected: usize, items: &[SuggestionItem]) -> String {
    let Some(selected_item) = items.get(selected) else {
        return String::new();
    };
    let (filter_kind, filter_name) = prefix_type_filter(selected_item);
    if matches!(filter_kind.as_ref(), "" | "special" | "auto-execute") {
        return String::new();
    }
    let names: Vec<String> = items
        .iter()
        .filter(|item| items_match_for_prefix_parts(item, filter_kind.as_ref(), filter_name.as_ref()))
        .map(|item| item.name.to_ascii_lowercase())
        .collect();
    if names.len() < 2 {
        return String::new();
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    longest_common_prefix(&refs)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabPrefix {
    /// Shared prefix of same-kind, prefix-matching rows (insert like a partial name).
    Partial(String),
    /// Insert the selected row as a full completion.
    Full(String),
}

/// Tab completion text, matching the old overlay: same-kind rows whose names
/// still prefix-match the query. One match inserts the row; several share a prefix.
pub fn tab_prefix_insertion(selected: usize, items: &[SuggestionItem], search: &str) -> Option<TabPrefix> {
    let selected_item = items.get(selected)?;
    let search = selected_item.query_term.as_deref().unwrap_or(search);
    // The legacy insertion path accepts the sole row before applying the
    // special/auto-execute common-prefix guard. This matters for generators
    // that directly return one action row; Tab accepts it exactly like a
    // mouse click, while multiple action rows still have no common prefix.
    if items.len() == 1 {
        return Some(TabPrefix::Full(selected_item.name.clone()));
    }
    if matches!(selected_item.kind.as_str(), "special" | "auto-execute") {
        return None;
    }
    let prefix_matches: Vec<&str> = items
        .iter()
        .filter(|item| items_match_for_prefix(item, selected_item))
        .map(|item| item.name.as_str())
        .filter(|name| search.is_empty() || match_prefix_bytes(name, search) > 0)
        .collect();
    if prefix_matches.len() == 1 {
        return Some(TabPrefix::Full(selected_item.name.clone()));
    }
    if prefix_matches.len() < 2 {
        return None;
    }
    let lower: Vec<String> = prefix_matches.iter().map(|name| name.to_ascii_lowercase()).collect();
    let refs: Vec<&str> = lower.iter().map(String::as_str).collect();
    let lcp = longest_common_prefix(&refs);
    if lcp.is_empty() {
        return None;
    }
    let shared: String = selected_item.name.chars().take(lcp.chars().count()).collect();
    if shared.is_empty() || shared == search {
        return None;
    }
    Some(TabPrefix::Partial(shared))
}

fn merge_runs(runs: Vec<TextRun>) -> Vec<TextRun> {
    let mut out: Vec<TextRun> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.kind == run.kind {
                last.text.push_str(&run.text);
                continue;
            }
        }
        out.push(run);
    }
    out
}

pub fn name_runs(name: &str, search: &str, fuzzy: bool, common_prefix: &str) -> Vec<TextRun> {
    let mut runs = if search.is_empty() {
        vec![TextRun {
            text: name.to_string(),
            kind: RunKind::Text,
        }]
    } else if fuzzy {
        match fuzzy_indexes(name, search) {
            Some(indexes) => runs_for_fuzzy_indexes(name, &indexes),
            None => vec![TextRun {
                text: name.to_string(),
                kind: RunKind::Text,
            }],
        }
    } else if match_prefix_bytes(name, search) > 0 {
        let bytes = match_prefix_bytes(name, search);
        vec![
            TextRun {
                text: name[..bytes].to_string(),
                kind: RunKind::Match,
            },
            TextRun {
                text: name[bytes..].to_string(),
                kind: RunKind::Text,
            },
        ]
    } else {
        vec![TextRun {
            text: name.to_string(),
            kind: RunKind::Text,
        }]
    };
    runs = merge_runs(runs);

    if !common_prefix.is_empty() && common_prefix != "-" {
        let search_len = search.chars().count();
        let prefix_len = common_prefix.chars().count();
        if runs.first().is_some_and(|run| run.kind == RunKind::Text) {
            if let Some(rest) = underline_prefix(&runs, 0, prefix_len) {
                runs = rest;
            }
        } else if runs.len() > 1 && runs[0].kind == RunKind::Match && runs[1].kind == RunKind::Text {
            let remain = prefix_len.saturating_sub(search_len);
            if remain > 0 {
                if let Some(rest) = underline_prefix(&runs, 1, remain) {
                    runs = rest;
                }
            }
        }
        runs = merge_runs(runs);
    }
    runs
}

fn runs_for_fuzzy_indexes(name: &str, indexes: &[usize]) -> Vec<TextRun> {
    if indexes.is_empty() {
        return vec![TextRun {
            text: name.to_string(),
            kind: RunKind::Text,
        }];
    }
    let marked: std::collections::HashSet<usize> = indexes.iter().copied().collect();
    let mut runs = Vec::new();
    let mut span_start = 0;
    let mut span_kind = RunKind::Text;
    let mut started = false;
    for (char_index, (byte_index, _)) in name.char_indices().enumerate() {
        let kind = if marked.contains(&char_index) {
            RunKind::Match
        } else {
            RunKind::Text
        };
        if !started {
            span_start = byte_index;
            span_kind = kind;
            started = true;
            continue;
        }
        if kind != span_kind {
            runs.push(TextRun {
                text: name[span_start..byte_index].to_string(),
                kind: span_kind,
            });
            span_start = byte_index;
            span_kind = kind;
        }
    }
    if started {
        runs.push(TextRun {
            text: name[span_start..].to_string(),
            kind: span_kind,
        });
    }
    runs
}

fn underline_prefix(runs: &[TextRun], index: usize, char_count: usize) -> Option<Vec<TextRun>> {
    if char_count == 0 || index >= runs.len() || runs[index].kind != RunKind::Text {
        return None;
    }
    let text = &runs[index].text;
    let mut bytes = 0;
    for (count, ch) in text.chars().enumerate() {
        if count >= char_count {
            break;
        }
        bytes += ch.len_utf8();
    }
    let bytes = bytes.min(text.len());
    if bytes == 0 {
        return None;
    }
    let prefix = text[..bytes].to_string();
    let rest = text[bytes..].to_string();
    let mut out = Vec::new();
    out.extend_from_slice(&runs[..index]);
    out.push(TextRun {
        text: prefix,
        kind: RunKind::Prefix,
    });
    if !rest.is_empty() {
        out.push(TextRun {
            text: rest,
            kind: RunKind::Text,
        });
    }
    out.extend_from_slice(&runs[index + 1..]);
    Some(out)
}

pub struct SuggestionList {
    pub state: Entity<OverlayState>,
    scroll_handle: UniformListScrollHandle,
    /// Snapshot of the selection and viewport used for the last smart-scroll
    /// check. The old react-window list re-ran its smart scroll after a resize
    /// as well as after keyboard navigation; keeping the visible range here
    /// lets the native list do the same without centering every row change.
    scroll_snapshot: Option<(usize, usize, usize, usize, usize)>,
    /// Last size handed to GPUI's native window. Repeating the same resize on
    /// every caret/frame update feeds back into AppKit's resize callbacks.
    pub(crate) last_requested_size: Option<(f32, f32)>,
    /// Last frame handed to AppKit. Position updates are frequent while the
    /// terminal caret moves, so avoid enqueueing an identical native frame.
    /// This is reset when the window is parked so the next show still issues a
    /// fresh request (and can order the hidden window to the front).
    pub(crate) last_requested_frame: Option<(f32, f32, f32, f32)>,
}

impl SuggestionList {
    pub fn new(state: Entity<OverlayState>) -> Self {
        Self {
            state,
            scroll_handle: UniformListScrollHandle::new(),
            scroll_snapshot: None,
            last_requested_size: None,
            last_requested_frame: None,
        }
    }
}

impl Render for SuggestionList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let overlay = self.state.read(cx);
        let has_current_arg = overlay.has_current_arg();
        if !overlay.visible || (overlay.items.is_empty() && !overlay.loading && !has_current_arg) {
            self.scroll_snapshot = None;
            return div().id("ec-overlay-hidden").w(px(1.)).h(px(1.));
        }

        let theme = overlay.theme;
        let selected = overlay.selected;
        let count = overlay.items.len();
        let font_family = overlay.font_family.clone();
        let font_size = overlay.effective_font_size();
        let row_height = overlay.effective_row_height();
        let list_width = overlay.effective_list_width();
        let max_list_height = overlay.effective_max_list_height();
        let state = self.state.clone();
        let click = overlay.on_click_insert.clone();
        let disable_dev = overlay.on_disable_dev_mode.clone();
        let fuzzy = overlay.effective_fuzzy_search;
        // With no suggestions the WebView renders parser context in the normal
        // description row; a separate popout only exists beside rows.
        let popout = overlay.description_popout && count > 0;
        let on_left = overlay.description_on_left;
        let is_above = overlay.is_above_cursor;
        let shaking = overlay.shaking;
        let loading = overlay.loading;
        let show_hint = !overlay.always_show_description && !loading;
        let show_dev = overlay.show_dev_banner;
        let debug_window = overlay.debug_window;
        let common_prefix = overlay.common_prefix.clone();
        // The bottom Description falls back to currentArg, but the old
        // popout deliberately describes only the selected suggestion.
        let selected_description = selected_item_description(overlay.selected_item());
        // The old WebView uses `size.itemSize * 0.75` for both dimensions. Do
        // not clamp this to the default 15px: custom row heights must keep the
        // same icon-to-row ratio as the old overlay.
        let icon_size = icon_size_for_row(row_height);
        let visible_rows = suggestion_visible_rows(count, row_height, max_list_height, popout, loading);
        let list_h = visible_rows as f32 * row_height;
        if count > 0 {
            let (first_visible, last_visible) = {
                let scroll_state = self.scroll_handle.0.borrow();
                (
                    scroll_state.base_handle.top_item(),
                    scroll_state.base_handle.bottom_item(),
                )
            };
            let snapshot = (selected, count, visible_rows, first_visible, last_visible);
            if self.scroll_snapshot != Some(snapshot) {
                if let Some(strategy) = smart_scroll_strategy(selected, first_visible, last_visible) {
                    self.scroll_handle.scroll_to_item(selected, strategy);
                }
                self.scroll_snapshot = Some(snapshot);
            }
        }

        let footer_h = row_height;
        // Built before the rows so they know whether the last one reaches the
        // bottom of the card.
        let footer: Option<AnyElement> = if count == 0 && has_current_arg {
            Some(
                description_only_bar(
                    &overlay.current_arg_name,
                    &overlay.current_arg_description,
                    theme,
                    row_height,
                )
                .into_any_element(),
            )
        } else if !popout {
            Some(
                description_bar(
                    &selected_description,
                    &overlay.current_arg_name,
                    &overlay.current_arg_description,
                    theme,
                    footer_h,
                    show_hint,
                    loading,
                    &overlay.description_hint,
                )
                .into_any_element(),
            )
        } else if loading && count > 0 {
            Some(loading_icon(theme, footer_h).into_any_element())
        } else {
            None
        };
        let radius = card_radius(font_size);
        let last_row = count.saturating_sub(1);
        let has_footer = footer.is_some();

        let mut column = div()
            .id("ec-list-column")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(list_width))
            .overflow_hidden()
            .rounded(px(card_radius(font_size)))
            .shadow(legacy_card_shadow())
            .bg(rgb(theme.background));
        if count > 0 {
            column = column.child(
                uniform_list("ec-suggestions", count, {
                    let common_prefix = common_prefix.clone();
                    let state = state.clone();
                    let click = click.clone();
                    let suggestion_font_family = font_family.clone();
                    move |range, _window, cx| {
                        let overlay = state.read(cx);
                        range
                            .filter_map(|ix| {
                                let item = overlay.items.get(ix)?;
                                let search_term = item.query_term.as_deref().unwrap_or(&overlay.match_term);
                                let corners = row_corner_radii(ix, last_row, radius, has_footer);
                                Some(suggestion_row(
                                    item,
                                    ix == overlay.selected,
                                    search_term,
                                    fuzzy,
                                    common_prefix.as_ref(),
                                    theme,
                                    row_height,
                                    icon_size,
                                    font_size,
                                    corners,
                                    suggestion_font_family.clone(),
                                    state.clone(),
                                    click.clone(),
                                    ix,
                                ))
                            })
                            .collect()
                    }
                })
                .track_scroll(self.scroll_handle.clone())
                .h(px(list_h)),
            );
        }
        if let Some(footer) = footer {
            column = column.child(footer);
        }
        let list_column = div().when(shaking, |this| this.ml(px(3.))).child(column);

        let mut body = div()
            .id("ec-overlay-body")
            .flex()
            .flex_row()
            .items_start()
            .when(is_above, |this| this.items_end())
            .gap(px(layout_gap(font_size)))
            .p(px(layout_pad(font_size)));
        if popout && on_left {
            body = body.child(description_popout(
                &selected_description,
                theme,
                max_list_height,
                show_hint,
                &overlay.description_hint,
            ));
        }
        body = body.child(list_column);
        if popout && !on_left {
            body = body.child(description_popout(
                &selected_description,
                theme,
                max_list_height,
                show_hint,
                &overlay.description_hint,
            ));
        }

        let list_width_for_banner = list_width;

        div()
            .id("ec-overlay")
            .flex()
            .flex_col()
            .font_family(font_family)
            .text_size(px(font_size))
            .overflow_hidden()
            .w_full()
            .h_full()
            .when(debug_window, |this| this.bg(rgb(DEBUG_WINDOW_FILL)))
            .when(!loading && is_above && show_dev, {
                let disable_dev = disable_dev.clone();
                let state = state.clone();
                move |this| this.child(dev_banner(list_width_for_banner, state, disable_dev))
            })
            .child(if loading {
                loading_icon(theme, row_height).into_any_element()
            } else {
                body.into_any_element()
            })
            .when(!loading && !is_above && show_dev, |this| {
                this.child(dev_banner(list_width_for_banner, state, disable_dev))
            })
    }
}

#[allow(clippy::too_many_arguments)]
fn suggestion_row(
    item: &SuggestionItem,
    is_selected: bool,
    search_term: &str,
    fuzzy: bool,
    common_prefix: &str,
    theme: OverlayTheme,
    row_height: f32,
    icon_size: f32,
    font_size: f32,
    // Top and bottom radii this row must reproduce to keep the card's corners
    // intact, since GPUI's overflow clip does not follow a border radius.
    corners: (f32, f32),
    font_family: String,
    state: Entity<OverlayState>,
    click: Option<Arc<dyn Fn(ClickInsert) + Send + Sync>>,
    ix: usize,
) -> AnyElement {
    // The WebView used brightness filters on the selected row and again on
    // match marks, with a brighter child span. GPUI has no CSS filter stack,
    // so apply the equivalent channel multipliers to the affected colors.
    let row_brightness = if is_selected { 0.95 } else { 1.0 };
    // The legacy row only set a background when active; an unselected row let
    // the card show through, which is also what keeps the rounded corners.
    let bg = is_selected.then(|| brightness(rgb(theme.selected), row_brightness));
    let text = brightness(
        if is_selected {
            rgb(theme.selected_text)
        } else {
            rgb(theme.text)
        },
        row_brightness,
    );
    let mut match_bg = brightness(
        if is_selected {
            rgb(theme.selected_match_bg)
        } else {
            rgb(theme.match_bg)
        },
        row_brightness * 0.95,
    );
    match_bg.a = 0.8;
    let match_text = brightness(text, 0.95 * 1.25);
    let title_name = item.display_name.as_deref().unwrap_or(&item.name);
    let runs = name_runs(title_name, search_term, fuzzy, common_prefix);
    let mut title = div()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .font_family(font_family)
        .text_color(text);
    for run in runs {
        title = title.child(match run.kind {
            RunKind::Match => div().bg(match_bg).text_color(match_text).child(run.text),
            RunKind::Prefix => div().underline().text_color(text).child(run.text),
            RunKind::Text => div().text_color(text).child(run.text),
        });
    }
    if !item.args_hint.is_empty() {
        title = title.child(
            div()
                .text_color(text)
                .opacity(0.5)
                .child(format!(" {}", item.args_hint)),
        );
    }
    let icon = item.icon_png.clone();
    div()
        .id(("ec-suggestion", ix))
        .flex()
        .flex_row()
        .items_center()
        // `uniform_list` lays each row out as a layout root, where an `auto`
        // width shrinks to fit. Without this the selected background would end
        // after the title instead of spanning the card like the legacy row.
        .w_full()
        .overflow_hidden()
        .rounded_t(px(corners.0))
        .rounded_b(px(corners.1))
        .pl(px(row_pad_left(font_size)))
        .h(px(row_height))
        .when_some(bg, |this, bg| this.bg(bg))
        .child(row_icon(
            &item.kind,
            icon,
            item.icon_identifier.as_deref(),
            icon_size,
        ))
        .child(div().ml(px(5.)).overflow_hidden().child(title))
        // React's Suggestion uses onClick (mouse-up), not mouse-down. Besides
        // matching the old acceptance timing this avoids accepting a row when
        // the user presses and drags out of it before releasing.
        .on_click(move |_event, _window, cx| {
            let payload = state.update(cx, |overlay, cx| {
                overlay.selected = ix;
                overlay.has_changed_index = true;
                cx.notify();
                overlay
                    .items
                    .get(ix)
                    .map(|item| click_insert_for(item, &overlay.search_term))
            });
            if let (Some(insert), Some(payload)) = (click.as_ref(), payload) {
                insert(payload);
            }
        })
        .into_any_element()
}

/// GPUI clips `overflow_hidden` to a plain rectangle rather than following the
/// border radius, so a row that paints a background over the card has to
/// reproduce whichever card corners it covers. Only the first row reaches the
/// top, and the last row reaches the bottom only when no footer follows it.
fn row_corner_radii(ix: usize, last_row: usize, radius: f32, has_footer: bool) -> (f32, f32) {
    (
        if ix == 0 { radius } else { 0.0 },
        if ix == last_row && !has_footer { radius } else { 0.0 },
    )
}

fn brightness(mut color: Rgba, factor: f32) -> Rgba {
    color.r = (color.r * factor).clamp(0.0, 1.0);
    color.g = (color.g * factor).clamp(0.0, 1.0);
    color.b = (color.b * factor).clamp(0.0, 1.0);
    color
}

fn click_insert_for(item: &SuggestionItem, raw_search_term: &str) -> ClickInsert {
    ClickInsert {
        name: item.name.clone(),
        description: item.description.clone(),
        search: raw_search_term.to_string(),
        kind: item.kind.clone(),
        args_hint: item.args_hint.clone(),
        insert_value: item.insert_value.clone(),
        display_name: item.display_name.clone(),
        primary_name: item.primary_name.clone(),
        separator_to_add: item.separator_to_add.clone(),
        should_add_space: item.should_add_space,
        hidden: item.hidden,
        priority: item.priority,
        icon_identifier: item.icon_identifier.clone(),
        original_type: item.original_type.clone(),
        query_term: item.query_term.clone(),
    }
}

fn row_icon(kind: &str, png: Option<Arc<Image>>, icon_identifier: Option<&str>, size: f32) -> impl IntoElement {
    if let Some(icon) = icon_identifier.and_then(|identifier| identifier_icon_element(identifier, size)) {
        return icon;
    }
    if kind == "history" && png.is_none() {
        return history_icon(size).into_any_element();
    }
    if let Some(image) = png {
        return png_icon_element(image, size).into_any_element();
    }
    if let Some(icon) = fallback_template_icon(kind, size) {
        return icon;
    }
    named_icon_element(kind, size).into_any_element()
}

/// `SuggestionIcon` used template tiles for these two legacy suggestion
/// types. They are not ordinary `box` icons, so keep the badge and color in
/// the native fallback path when a spec did not provide an explicit icon.
fn fallback_template_icon(kind: &str, size: f32) -> Option<AnyElement> {
    let identifier = match kind {
        "shortcut" => "fig://template?color=3498db&badge=💡",
        "mixin" => "fig://template?color=628dad&badge=➡️",
        _ => return None,
    };
    identifier_icon_element(identifier, size)
}

fn history_icon(size: f32) -> impl IntoElement {
    div()
        .w(px(size))
        .h(px(size))
        .min_w(px(size))
        .min_h(px(size))
        .flex_shrink_0()
        .rounded(px(size * 0.25))
        .bg(rgb(0x6b7280))
        .flex()
        .items_center()
        .justify_center()
        .child(history_icon_image_element(size * 0.74))
}

#[cfg(test)]
fn item_description(item: Option<&SuggestionItem>, current_arg_name: &str, current_arg_description: &str) -> String {
    let selected = selected_item_description(item);
    if !selected.is_empty() {
        return selected;
    }
    let name = current_arg_name.trim();
    let description = current_arg_description.trim();
    match (name.is_empty(), description.is_empty()) {
        (false, false) => format!("{name}: {description}"),
        (false, true) => name.to_string(),
        (true, false) => description.to_string(),
        (true, true) => String::new(),
    }
}

/// Description used by the selected-row popout. This intentionally does not
/// fall back to currentArg; the WebView's popout showed `No description` when
/// the selected suggestion itself had no description.
fn selected_item_description(item: Option<&SuggestionItem>) -> String {
    let Some(item) = item else {
        return String::new();
    };
    let trimmed = item.description.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if matches!(item.kind.as_str(), "file" | "folder") {
        return item.kind.clone();
    }
    String::new()
}

fn suggestion_footer_height(item_count: usize, row_height: f32, popout: bool, loading: bool) -> f32 {
    if item_count == 0 {
        0.0
    } else if popout {
        if loading { row_height } else { 0.0 }
    } else {
        row_height
    }
}

/// Number of full-size rows that fit while keeping the bottom Description in
/// the old card's max-height budget. `react-window` receives the remaining
/// height after the footer; it is not allowed to claim the whole max height
/// and then push the footer below it.
fn suggestion_visible_rows(
    item_count: usize,
    row_height: f32,
    max_list_height: f32,
    popout: bool,
    loading: bool,
) -> usize {
    if item_count == 0 || row_height <= 0.0 {
        return 0;
    }
    let footer = suggestion_footer_height(item_count, row_height, popout, loading);
    let available = (max_list_height - footer).max(row_height);
    ((available / row_height).floor() as usize).clamp(1, item_count)
}

fn description_only_bar(name: &str, description: &str, theme: OverlayTheme, row_height: f32) -> impl IntoElement {
    let name = name.trim();
    let description = description.trim();
    let stacked = !name.is_empty() && !description.is_empty();
    let height = row_height * if stacked { 2.0 } else { 1.0 };
    let mut content = div()
        .id("ec-description-only-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .overflow_x_scroll()
        .overflow_y_hidden()
        .whitespace_nowrap();
    if !name.is_empty() {
        let title = if stacked { format!("{name}: ") } else { name.to_string() };
        content = content.child(div().flex_none().font_weight(FontWeight::BOLD).child(title));
    }
    if !description.is_empty() {
        content = content.child(div().flex_none().child(description.to_string()));
    }
    div()
        .id("ec-description-only")
        .flex()
        .items_center()
        .flex_none()
        .h(px(height))
        .px(px(8.))
        // No background: the card already paints this exact colour, and an
        // opaque child would square off the rounded corner it covers.
        .italic()
        .text_color(rgb(theme.muted))
        .overflow_hidden()
        .child(content)
}

#[allow(clippy::too_many_arguments)]
fn description_bar(
    selected_description: &str,
    current_arg_name: &str,
    current_arg_description: &str,
    theme: OverlayTheme,
    height: f32,
    show_hint: bool,
    loading: bool,
    hint: &str,
) -> impl IntoElement {
    let selected_description = selected_description.trim();
    let current_arg_name = current_arg_name.trim();
    let current_arg_description = current_arg_description.trim();
    let use_current_arg = selected_description.is_empty();
    let empty = use_current_arg && current_arg_name.is_empty() && current_arg_description.is_empty();
    let mut content = div()
        .id("ec-description-scroll")
        .flex()
        .flex_row()
        .flex_1()
        .overflow_x_scroll()
        .overflow_y_hidden()
        .whitespace_nowrap();
    if !selected_description.is_empty() {
        content = content.child(selected_description.to_string());
    } else {
        if !current_arg_name.is_empty() {
            content = content.child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .child(current_arg_name.to_string()),
            );
        }
        if !current_arg_name.is_empty() && !current_arg_description.is_empty() {
            content = content.child(div().flex_none().child(": "));
        }
        if !current_arg_description.is_empty() {
            content = content.child(div().flex_none().child(current_arg_description.to_string()));
        } else if empty {
            content = content.child("No description");
        }
    }
    div()
        .id("ec-description")
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .flex_none()
        .h(px(height))
        .px(px(5.))
        .border_t_1()
        .border_color(rgb(theme.border))
        .italic()
        .text_color(rgb(if empty { 0x8c8c8c } else { theme.muted }))
        .overflow_hidden()
        .child(content)
        .when(loading, |this| this.child(loading_icon(theme, height)))
        .when(show_hint && !loading, |this| this.child(hint_chip(height, hint)))
}

fn description_popout(
    description: &str,
    theme: OverlayTheme,
    max_height: f32,
    show_hint: bool,
    hint: &str,
) -> impl IntoElement {
    let empty = description.is_empty();
    let text = if empty {
        "No description".to_string()
    } else {
        description.to_string()
    };
    div()
        .id("ec-description-popout")
        .flex()
        .flex_col()
        .flex_none()
        .w(px(POPOUT_WIDTH))
        .max_h(px(max_height))
        .pt(px(2.))
        .pb(px(4.))
        .rounded(px(POPOUT_RADIUS))
        .shadow(legacy_card_shadow())
        .bg(rgb(theme.background))
        .italic()
        .text_color(rgb(if empty { 0x8c8c8c } else { theme.muted }))
        .overflow_hidden()
        .child(
            div()
                .id("ec-description-popout-scroll")
                .flex_1()
                .max_h(px((max_height - 10.0).max(0.0)))
                .overflow_y_scroll()
                .overflow_x_hidden()
                .pl(px(6.))
                .pr(px(4.))
                .child(text),
        )
        .when(show_hint, |this| {
            this.child(
                div()
                    .flex()
                    .justify_end()
                    .mt(px(4.))
                    .mx(px(4.))
                    .child(div().flex_none().bg(rgb(theme.border)).child(hint_chip(24., hint))),
            )
        })
}

fn hint_chip(height: f32, hint: &str) -> impl IntoElement {
    let hint = if hint.trim().is_empty() { "⌃k" } else { hint }.to_string();
    div()
        .rounded(px(3.))
        .px(px(4.))
        .text_size(px((height * 0.5).max(9.)))
        .italic()
        .child(hint)
}

fn loading_icon(theme: OverlayTheme, _row_height: f32) -> impl IntoElement {
    div()
        .id("ec-loading")
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .w(px(40.0))
        .h(px(24.0))
        .px(px(8.))
        .text_color(rgb(theme.selected_text))
        // This used to be a repeating animation. A stalled completion stream
        // could then keep the GPUI run loop repainting forever; a static marker
        // communicates the same state without keeping the process hot.
        .child(div().child(LOADING_DOTS))
}

fn dev_banner(
    width: f32,
    state: Entity<OverlayState>,
    on_disable: Option<Arc<dyn Fn() + Send + Sync>>,
) -> impl IntoElement {
    div()
        .id("ec-dev-banner")
        .m(px(4.))
        .w(px((width - 20.0).max(120.0)))
        .rounded(px(4.))
        .px(px(10.))
        .py(px(8.))
        .bg(rgb(0xf59e0b))
        .text_color(rgb(0x000000))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(div().font_weight(FontWeight::BOLD).child("Developer mode enabled!"))
        .child(
            div()
                .text_size(px(11.))
                .child("Loading specs from disk. Disable with either"),
        )
        .child(
            div()
                .text_size(px(10.))
                .pl(px(8.))
                .child("• Ctrl + C in the dev mode process"),
        )
        .child(
            div()
                .text_size(px(10.))
                .pl(px(8.))
                .underline()
                .child("• Click to disable")
                .on_mouse_down(gpui::MouseButton::Left, move |_event, _window, cx| {
                    state.update(cx, |overlay, cx| {
                        overlay.show_dev_banner = false;
                        cx.notify();
                    });
                    if let Some(disable) = &on_disable {
                        disable();
                    }
                }),
        )
}

/// Outer window size that matches the old overlay chrome (padding, popout, banner, footer).
#[allow(clippy::too_many_arguments)]
pub fn overlay_content_size(
    item_count: usize,
    row_height: f32,
    font_size: f32,
    list_width: f32,
    max_list_height: f32,
    popout: bool,
    show_dev_banner: bool,
    loading: bool,
) -> (f32, f32) {
    overlay_content_size_with_context(
        item_count,
        row_height,
        font_size,
        list_width,
        max_list_height,
        popout,
        show_dev_banner,
        loading,
        0,
    )
}

/// Variant used by the desktop controller when the parser has a current
/// argument but no completion rows. `description_rows` is normally 0; a
/// description-only state uses one or two rows just like the WebView.
#[allow(clippy::too_many_arguments)]
pub fn overlay_content_size_with_context(
    item_count: usize,
    row_height: f32,
    font_size: f32,
    list_width: f32,
    max_list_height: f32,
    popout: bool,
    show_dev_banner: bool,
    loading: bool,
    description_rows: usize,
) -> (f32, f32) {
    // The WebView replaced the entire autocomplete card with its compact
    // loading indicator, even when stale rows were still present in state.
    if loading {
        return (40.0, 24.0);
    }
    let visible_rows = if item_count == 0 {
        if description_rows > 0 { description_rows } else { 0 }
    } else {
        suggestion_visible_rows(item_count, row_height, max_list_height, popout, loading)
    };
    let list_h = visible_rows as f32 * row_height;
    let footer = suggestion_footer_height(item_count, row_height, popout, loading);
    let border = CARD_BORDER * 2.0;
    let column_h = list_h + footer + border;
    let popout_h = if popout {
        max_list_height.min(column_h) + border
    } else {
        0.0
    };
    let inner_h = column_h.max(popout_h);
    let inner_w = list_width
        + border
        + if popout {
            POPOUT_WIDTH + border + layout_gap(font_size)
        } else {
            0.0
        };
    let banner = if show_dev_banner { DEV_BANNER_HEIGHT } else { 0.0 };
    let pad = layout_pad(font_size) * 2.0;
    (inner_w + pad, inner_h + pad + banner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_prefix_is_case_insensitive() {
        assert_eq!(match_prefix_bytes("checkout", "ch"), 2);
        assert_eq!(match_prefix_bytes("Checkout", "ch"), 2);
        assert_eq!(match_prefix_bytes("src/main.rs", "src/m"), 5);
        assert_eq!(match_prefix_bytes("checkout", "git"), 0);
        assert_eq!(match_prefix_bytes("文件.txt", "文"), "文".len());
    }

    #[test]
    fn icon_box_keeps_the_old_row_ratio() {
        // CSS `0 0 3px` is sigma 1.5, and GPUI blurs by sigma directly.
        assert_eq!(LEGACY_SHADOW_SIGMA, 1.5);
        assert_eq!(icon_size_for_row(DEFAULT_ROW_HEIGHT), ICON_SIZE);
        assert_eq!(icon_size_for_row(16.0), 12.0);
    }

    #[test]
    fn only_rows_touching_a_card_corner_round_themselves() {
        // Footer present: the list owns the top corners only.
        assert_eq!(row_corner_radii(0, 4, 3.2, true), (3.2, 0.0));
        assert_eq!(row_corner_radii(2, 4, 3.2, true), (0.0, 0.0));
        assert_eq!(row_corner_radii(4, 4, 3.2, true), (0.0, 0.0));
        // Popout mode drops the footer, so the last row reaches the bottom.
        assert_eq!(row_corner_radii(4, 4, 3.2, false), (0.0, 3.2));
        // A lone row owns all four corners.
        assert_eq!(row_corner_radii(0, 0, 3.2, false), (3.2, 3.2));
    }

    #[test]
    fn debug_window_fill_is_opaque_red() {
        assert_eq!(DEBUG_WINDOW_FILL, 0xff_00_00);
        let src = include_str!("list.rs");
        assert!(
            src.contains("when(debug_window") && src.contains("DEBUG_WINDOW_FILL"),
            "debug mode must paint the overlay window root, not only the suggestion card"
        );
    }

    #[test]
    fn rem_spacings_use_the_font_size_not_a_16px_basis() {
        // `:root { font-size: 12.8px }` made every Tailwind rem 0.8x what a
        // browser default would give: `p-1` is 3.2px, not 4px.
        assert_eq!(layout_pad(DEFAULT_FONT_SIZE), 3.2);
        assert_eq!(layout_gap(DEFAULT_FONT_SIZE), 4.8);
        assert_eq!(card_radius(DEFAULT_FONT_SIZE), 3.2);
        assert_eq!(row_pad_left(DEFAULT_FONT_SIZE), 4.8);
        // The popout radius was a literal `4px` and must not scale.
        assert_eq!(POPOUT_RADIUS, 4.0);
        // A larger font scales the chrome with it, as `rem` did.
        assert_eq!(layout_pad(16.0), 4.0);
    }

    #[test]
    fn smart_scroll_only_moves_when_selection_leaves_viewport() {
        assert_eq!(smart_scroll_strategy(3, 0, 6), None);
        assert_eq!(smart_scroll_strategy(0, 2, 6), Some(ScrollStrategy::Top));
        assert_eq!(smart_scroll_strategy(9, 2, 6), Some(ScrollStrategy::Bottom));
    }

    #[test]
    fn loading_marker_is_static() {
        assert_eq!(LOADING_DOTS, "···");
    }

    #[test]
    fn fuzzy_runs_highlight_subsequence() {
        let runs = name_runs("checkout", "ckt", true, "");
        let matched: String = runs
            .iter()
            .filter(|run| run.kind == RunKind::Match)
            .map(|run| run.text.as_str())
            .collect();
        assert_eq!(matched, "ckt");
    }

    #[test]
    fn fuzzy_indexes_prefer_word_boundaries_and_consecutive_matches() {
        assert_eq!(fuzzy_indexes("fooBar", "fb"), Some(vec![0, 3]));
        assert_eq!(fuzzy_indexes("aXab", "ab"), Some(vec![0, 3]));
        assert_eq!(fuzzy_indexes("checkout", ""), Some(Vec::new()));
        assert_eq!(fuzzy_indexes("checkout", "z"), None);
    }

    #[test]
    fn description_falls_back_to_current_arg_name_and_description() {
        assert_eq!(item_description(None, "branch", "Branch name"), "branch: Branch name");
        assert_eq!(item_description(None, "branch", ""), "branch");
        assert_eq!(item_description(None, "", "Branch name"), "Branch name");
    }

    #[test]
    fn popout_description_does_not_use_current_arg_fallback() {
        let item = item("status", "subcommand");
        assert_eq!(selected_item_description(Some(&item)), "");
        assert_eq!(item_description(Some(&item), "current", "context"), "current: context");
    }

    #[test]
    fn visible_rows_reserve_space_for_the_description_footer() {
        // The old card's 140px max contains six 20px rows plus its 20px
        // Description footer. The seventh row must not push that footer out.
        assert_eq!(suggestion_visible_rows(100, 20.0, 140.0, false, false), 6);
        assert_eq!(suggestion_visible_rows(100, 20.0, 140.0, true, false), 7);
        assert_eq!(suggestion_visible_rows(100, 20.0, 140.0, true, true), 6);
    }

    #[test]
    fn shortcut_and_mixin_use_template_icon_fallbacks() {
        assert!(fallback_template_icon("shortcut", ICON_SIZE).is_some());
        assert!(fallback_template_icon("mixin", ICON_SIZE).is_some());
        assert!(fallback_template_icon("arg", ICON_SIZE).is_none());
    }

    #[test]
    fn prefix_runs_highlight_query_then_underline_rest_of_common_prefix() {
        let runs = name_runs("checkout", "ch", false, "check");
        assert_eq!(runs[0].kind, RunKind::Match);
        assert_eq!(runs[0].text, "ch");
        assert_eq!(runs[1].kind, RunKind::Prefix);
        assert_eq!(runs[1].text, "eck");
    }

    #[test]
    fn common_prefix_uses_same_kind_rows() {
        let items = vec![
            SuggestionItem {
                name: "checkout".into(),
                kind: "subcommand".into(),
                ..SuggestionItem::default()
            },
            SuggestionItem {
                name: "cherry-pick".into(),
                kind: "subcommand".into(),
                ..SuggestionItem::default()
            },
            SuggestionItem {
                name: "--help".into(),
                kind: "option".into(),
                ..SuggestionItem::default()
            },
        ];
        assert_eq!(common_prefix_for(0, &items), "che");
    }

    #[test]
    fn common_prefix_does_not_clone_the_selected_row() {
        let src = include_str!("list.rs");
        let start = src.find("pub fn common_prefix_for").expect("common_prefix_for");
        let end = src[start..].find("pub enum TabPrefix").expect("TabPrefix") + start;
        let body = &src[start..end];
        assert!(
            body.contains("prefix_type_filter") && !body.contains("selected_item.clone()"),
            "common_prefix_for must not clone the selected SuggestionItem (icon png, strings)"
        );

        let render = src.find("impl Render for SuggestionList").expect("render");
        let render_end = src[render..].find("fn row_corner_radii").expect("next fn") + render;
        let render_body = &src[render..render_end];
        assert!(
            render_body.contains("overlay.common_prefix.clone()") && !render_body.contains("common_prefix_for("),
            "paint must use the prefix stored on OverlayState, not recompute it each frame"
        );
    }

    #[test]
    fn suggestion_row_does_not_clone_the_item_on_paint() {
        let src = include_str!("list.rs");
        let start = src.find("fn suggestion_row(").expect("suggestion_row");
        let end = src[start..].find("\nfn row_corner_radii").expect("next fn") + start;
        let body = &src[start..end];
        assert!(
            !body.contains("item.clone()") && !body.contains("click_item"),
            "paint must not clone SuggestionItem (strings + icon) for a click that may never happen"
        );
        assert!(
            body.contains(".items")
                && body.contains(".get(ix)")
                && body.contains("click_insert_for(item, &overlay.search_term)"),
            "click must look the row up from overlay state"
        );
    }

    #[test]
    fn fuzzy_name_runs_are_consecutive_spans() {
        let runs = name_runs("checkout", "ch", true, "");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].kind, RunKind::Match);
        assert_eq!(runs[0].text, "ch");
        assert_eq!(runs[1].kind, RunKind::Text);
        assert_eq!(runs[1].text, "eckout");

        let src = include_str!("list.rs");
        let start = src.find("fn runs_for_fuzzy_indexes").expect("runs_for_fuzzy_indexes");
        let end = src[start..].find("\nfn underline_prefix").expect("underline_prefix") + start;
        let body = &src[start..end];
        assert!(
            !body.contains("ch.to_string()") && body.contains("name[span_start..byte_index]"),
            "fuzzy highlight must slice consecutive spans, not allocate a String per character"
        );
    }

    #[test]
    fn auto_execute_row_uses_original_type_for_prefix_decoration() {
        let items = vec![
            SuggestionItem {
                name: "checkout".into(),
                kind: "auto-execute".into(),
                original_type: Some("subcommand".into()),
                ..SuggestionItem::default()
            },
            item("checkout", "subcommand"),
            item("cherry-pick", "subcommand"),
        ];
        assert_eq!(common_prefix_for(0, &items), "che");
        // Decoration parity must not turn Tab into execution when multiple
        // rows are present.
        assert_eq!(tab_prefix_insertion(0, &items, "ch"), None);
    }

    #[test]
    fn content_size_includes_footer_not_popout() {
        let (w, h) = overlay_content_size(3, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false);
        assert_eq!(w, 320.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
        assert_eq!(h, 60.0 + 20.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
    }

    #[test]
    fn content_size_adds_popout_width() {
        let (w, _) = overlay_content_size(3, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, false, false);
        assert_eq!(
            w,
            320.0
                + CARD_BORDER * 2.0
                + POPOUT_WIDTH
                + CARD_BORDER * 2.0
                + layout_gap(DEFAULT_FONT_SIZE)
                + layout_pad(DEFAULT_FONT_SIZE) * 2.0
        );
    }

    #[test]
    fn popout_height_tracks_a_short_list() {
        let (_, h) = overlay_content_size(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, false, false);
        assert_eq!(h, 20.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
    }

    #[test]
    fn content_size_loading_replaces_the_entire_card() {
        assert_eq!(
            overlay_content_size(3, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, true),
            (40.0, 24.0)
        );
        assert_eq!(
            overlay_content_size(3, 22.0, DEFAULT_FONT_SIZE, 320.0, 140.0, true, true, true),
            (40.0, 24.0)
        );
    }

    #[test]
    fn content_size_footer_tracks_row_height() {
        let (_, h20) = overlay_content_size(1, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false);
        let (_, h22) = overlay_content_size(1, 22.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false);
        assert_eq!(h22 - h20, 4.0);
    }

    #[test]
    fn content_size_keeps_current_arg_description_visible_without_rows() {
        let (_, one_line) =
            overlay_content_size_with_context(0, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false, 1);
        let (_, two_lines) =
            overlay_content_size_with_context(0, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, false, 2);
        assert_eq!(one_line, 20.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0);
        assert_eq!(
            two_lines,
            40.0 + CARD_BORDER * 2.0 + layout_pad(DEFAULT_FONT_SIZE) * 2.0
        );

        let loading =
            overlay_content_size_with_context(0, 20.0, DEFAULT_FONT_SIZE, 320.0, 140.0, false, false, true, 2);
        assert_eq!(loading, (40.0, 24.0));
    }

    fn item(name: &str, kind: &str) -> SuggestionItem {
        SuggestionItem {
            name: name.into(),
            kind: kind.into(),
            ..SuggestionItem::default()
        }
    }

    #[test]
    fn mouse_payload_keeps_raw_search_for_shell_deletion() {
        let mut suggestion = item("checkout", "subcommand");
        suggestion.query_term = Some("co".into());
        suggestion.primary_name = Some("checkout".into());
        let click = click_insert_for(&suggestion, "'co");
        assert_eq!(click.search, "'co");
        assert_eq!(click.query_term.as_deref(), Some("co"));
        assert_eq!(click.primary_name.as_deref(), Some("checkout"));
    }

    #[test]
    fn tab_prefix_inserts_full_row_when_only_one_suggestion() {
        let items = vec![item("checkout", "subcommand")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "ch"),
            Some(TabPrefix::Full("checkout".into()))
        );
    }

    #[test]
    fn tab_prefix_accepts_a_single_auto_execute_row() {
        let items = vec![item("↪", "auto-execute")];
        assert_eq!(tab_prefix_insertion(0, &items, ""), Some(TabPrefix::Full("↪".into())));
    }

    #[test]
    fn tab_prefix_uses_same_kind_prefix_matches_and_original_case() {
        let items = vec![
            item("Checkout", "subcommand"),
            item("Cherry-pick", "subcommand"),
            item("--help", "option"),
        ];
        assert_eq!(
            tab_prefix_insertion(0, &items, "ch"),
            Some(TabPrefix::Partial("Che".into()))
        );
    }

    #[test]
    fn parent_directory_does_not_join_an_ordinary_file_prefix_set() {
        let items = vec![
            item("src/", "folder"),
            item("scripts/", "folder"),
            item("../", "folder"),
        ];
        assert_eq!(common_prefix_for(0, &items), "s");
        assert_eq!(tab_prefix_insertion(0, &items, "s"), None);
    }

    #[test]
    fn tab_prefix_uses_the_selected_rows_query_term_override() {
        let mut first = item("foobar", "arg");
        first.query_term = Some("foo".into());
        let items = vec![first, item("foobaz", "arg")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "scope@foo"),
            Some(TabPrefix::Partial("fooba".into()))
        );
    }

    #[test]
    fn tab_prefix_inserts_full_row_when_only_one_same_kind_prefix() {
        let items = vec![item("checkout", "subcommand"), item("--help", "option")];
        assert_eq!(
            tab_prefix_insertion(0, &items, "ch"),
            Some(TabPrefix::Full("checkout".into()))
        );
    }

    #[test]
    fn tab_prefix_returns_none_when_shared_prefix_equals_query() {
        let items = vec![item("checkout", "subcommand"), item("cherry-pick", "subcommand")];
        assert_eq!(tab_prefix_insertion(0, &items, "che"), None);
    }
}
