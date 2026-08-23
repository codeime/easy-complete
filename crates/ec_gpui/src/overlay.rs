use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Bounds, Entity, Pixels, Point, Size, WindowBounds, WindowHandle, WindowOptions, point, px,
    size,
};

use crate::list::{DEFAULT_FONT_SIZE, DEFAULT_MAX_LIST_HEIGHT, DEFAULT_ROW_HEIGHT, DEFAULT_WIDTH, SuggestionList};

use crate::{harden_overlay_window_handle, park_overlay_window_handle, set_overlay_frame_handle};

/// Title of the GPUI overlay window.
///
/// Linux finds the X11 client by this string (GPUI 0.2.2 has no X11
/// `window_handle`). macOS uses the same title when the `NSWindow` pointer is
/// missing. Keep it distinct from the settings window title (`Settings`).
pub const OVERLAY_WINDOW_TITLE: &str = "Easy Complete";

/// Shared overlay entity: visibility, selection, and the suggestion rows.
pub struct OverlayState {
    pub visible: bool,
    pub selected: usize,
    pub items: Vec<crate::list::SuggestionItem>,
    /// Shared prefix of same-kind rows. Recomputed when `items` or `selected`
    /// change so paint does not walk the list on every GPUI frame.
    pub common_prefix: Arc<str>,
    pub search_term: String,
    /// Normalized token used only for matching/highlighting. `search_term`
    /// remains the raw shell text so acceptance can delete exact bytes.
    pub match_term: String,
    /// The parser's current argument is useful even when it produced no rows.
    /// The WebView kept that context visible as a description-only overlay.
    pub current_arg_name: String,
    pub current_arg_description: String,
    pub theme: crate::list::OverlayTheme,
    pub font_family: String,
    /// When false, suggestion titles use the legacy Monaco fallback while
    /// descriptions keep GPUI's system UI font. A configured family applies
    /// to the whole overlay, matching the old CSS variable behavior.
    pub custom_font_family: bool,
    pub font_size: f32,
    pub row_height: f32,
    pub list_width: f32,
    pub max_list_height: f32,
    pub size_scale: f32,
    /// User-selected default. A spec/argument filterStrategy may override it
    /// for one result without mutating this preference.
    pub fuzzy_search: bool,
    /// Matching mode used to produce and highlight the currently displayed
    /// result. This is reset from `fuzzy_search` when settings are applied.
    pub effective_fuzzy_search: bool,
    pub description_popout: bool,
    pub description_on_left: bool,
    pub is_above_cursor: bool,
    pub always_show_description: bool,
    /// Human-readable binding for the description action. This is supplied by
    /// the desktop layer from the active settings and falls back to `⌃k`.
    pub description_hint: String,
    pub shaking: bool,
    pub loading: bool,
    pub history_mode: bool,
    /// Persisted `beta.history.mode`: `show`, `history_only`, or `off`.
    pub history_setting: String,
    pub only_show_on_tab: bool,
    pub first_token_completion: bool,
    pub scroll_wrap_around: bool,
    pub navigate_to_history: bool,
    pub insert_space_automatically: bool,
    pub show_dev_banner: bool,
    /// `ec debug autocomplete-window`: paint the window root so overlay
    /// bounds are visible, including transparent padding.
    pub debug_window: bool,
    pub suppress_until_shown: bool,
    /// Set after accepting a completion that does not open a new argument.
    /// The shell emits one buffer update for that insertion; hide that update
    /// so the panel does not immediately pop back over the accepted text.
    pub suppress_next_completion: bool,
    /// A no-op acceptance (the selected value already exactly matches the
    /// current token) can also produce an unchanged shell-buffer update. Tie
    /// that suppression to the exact input so it cannot hide the user's next
    /// real keystroke if the terminal emits no acknowledgement.
    suppress_unchanged_completion: Option<(String, u32)>,
    pub has_changed_index: bool,
    pub on_click_insert: Option<Arc<dyn Fn(crate::list::ClickInsert) + Send + Sync>>,
    pub on_disable_dev_mode: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl OverlayState {
    pub fn new() -> Self {
        Self {
            visible: false,
            selected: 0,
            items: Vec::new(),
            common_prefix: Arc::from(""),
            search_term: String::new(),
            match_term: String::new(),
            current_arg_name: String::new(),
            current_arg_description: String::new(),
            theme: crate::list::OverlayTheme::default(),
            font_family: "Monaco".into(),
            custom_font_family: false,
            font_size: DEFAULT_FONT_SIZE,
            row_height: DEFAULT_ROW_HEIGHT,
            list_width: DEFAULT_WIDTH as f32,
            max_list_height: DEFAULT_MAX_LIST_HEIGHT as f32,
            size_scale: 1.0,
            fuzzy_search: true,
            effective_fuzzy_search: true,
            description_popout: false,
            description_on_left: false,
            is_above_cursor: false,
            always_show_description: false,
            description_hint: "⌃k".into(),
            shaking: false,
            loading: false,
            history_mode: false,
            history_setting: "show".into(),
            only_show_on_tab: false,
            first_token_completion: false,
            scroll_wrap_around: false,
            navigate_to_history: false,
            insert_space_automatically: true,
            show_dev_banner: false,
            debug_window: false,
            suppress_until_shown: false,
            suppress_next_completion: false,
            suppress_unchanged_completion: None,
            has_changed_index: false,
            on_click_insert: None,
            on_disable_dev_mode: None,
        }
    }

    pub fn effective_font_size(&self) -> f32 {
        self.font_size * self.size_scale
    }

    pub fn effective_row_height(&self) -> f32 {
        self.row_height * self.size_scale
    }

    pub fn effective_list_width(&self) -> f32 {
        // The WebView derived its width from the persisted history setting;
        // toggling the temporary Ctrl-R history view did not resize the card.
        self.list_width * self.size_scale
    }

    pub fn effective_max_list_height(&self) -> f32 {
        self.max_list_height * self.size_scale
    }

    pub fn has_current_arg(&self) -> bool {
        !self.current_arg_name.trim().is_empty() || !self.current_arg_description.trim().is_empty()
    }

    /// Number of rows needed by the description-only state. The old WebView
    /// stacked the argument name and description when both were present.
    pub fn current_arg_rows(&self) -> usize {
        if self.current_arg_name.trim().is_empty() || self.current_arg_description.trim().is_empty() {
            usize::from(self.has_current_arg())
        } else {
            2
        }
    }

    pub fn set_current_arg(&mut self, name: impl Into<String>, description: impl Into<String>) {
        self.current_arg_name = name.into();
        self.current_arg_description = description.into();
    }

    /// Update the placement flags only when they actually changed. The
    /// desktop controller receives caret updates at typing frequency; issuing
    /// a GPUI notification for an identical layout needlessly redraws the
    /// native window.
    pub fn set_layout_flags(&mut self, on_left: bool, is_above: bool) -> bool {
        let changed = self.description_on_left != on_left || self.is_above_cursor != is_above;
        self.description_on_left = on_left;
        self.is_above_cursor = is_above;
        changed
    }

    pub fn set_suggestions(&mut self, items: Vec<crate::list::SuggestionItem>, search_term: String) {
        self.set_suggestions_with_match_term(items, search_term.clone(), search_term);
    }

    pub fn set_suggestions_with_match_term(
        &mut self,
        items: Vec<crate::list::SuggestionItem>,
        search_term: String,
        match_term: String,
    ) {
        let will_be_visible = !self.suppress_until_shown && (!items.is_empty() || self.has_current_arg());
        let keep = if self.has_changed_index && will_be_visible {
            self.selected_item().map(crate::list::selection_identity)
        } else {
            None
        };
        self.items = items;
        self.search_term = search_term;
        self.match_term = match_term;
        self.loading = false;
        if let Some(identity) = keep {
            if let Some(index) = self
                .items
                .iter()
                .position(|item| crate::list::selection_identity(item) == identity)
            {
                self.selected = index;
            } else {
                self.selected = 0;
                self.has_changed_index = false;
            }
        } else {
            self.selected = 0;
            if !will_be_visible {
                self.has_changed_index = false;
            }
        }
        self.visible = will_be_visible;
        self.refresh_common_prefix();
    }

    fn refresh_common_prefix(&mut self) {
        self.common_prefix = Arc::from(crate::list::common_prefix_for(self.selected, &self.items));
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.loading = false;
        self.shaking = false;
    }

    pub fn hide_until_shown(&mut self) {
        self.hide();
        self.suppress_until_shown = true;
    }

    pub fn mark_suppress_next_completion(&mut self) {
        self.suppress_next_completion = true;
    }

    pub fn take_suppress_next_completion(&mut self) -> bool {
        let suppress = self.suppress_next_completion;
        self.suppress_next_completion = false;
        suppress
    }

    pub fn mark_suppress_unchanged_completion(&mut self, buffer: String, cursor: u32) {
        self.suppress_unchanged_completion = Some((buffer, cursor));
    }

    /// Consume a post-insertion suppression marker. An unchanged-input marker
    /// is deliberately discarded without suppressing when the buffer differs,
    /// which prevents a no-op acceptance from swallowing a later keypress.
    pub fn take_suppress_completion_for(&mut self, buffer: &str, cursor: u32) -> bool {
        if self.take_suppress_next_completion() {
            self.suppress_unchanged_completion = None;
            return true;
        }
        self.suppress_unchanged_completion
            .take()
            .is_some_and(|(expected_buffer, expected_cursor)| expected_buffer == buffer && expected_cursor == cursor)
    }

    pub fn clear_suggestions(&mut self) {
        self.items.clear();
        self.selected = 0;
        self.search_term.clear();
        self.match_term.clear();
        self.current_arg_name.clear();
        self.current_arg_description.clear();
        self.has_changed_index = false;
        self.refresh_common_prefix();
    }

    pub fn dismiss(&mut self) {
        self.hide();
        self.suppress_until_shown = false;
        self.suppress_next_completion = false;
        self.suppress_unchanged_completion = None;
        self.clear_suggestions();
    }

    pub fn move_selection(&mut self, delta: i32) {
        let _ = self.move_selection_with_wrap(delta, true);
    }

    /// Returns `false` only when wrap is off and the selection would move above
    /// the first row (the previous WebView hid on Up-from-top). Down past the
    /// last row stays on the last item.
    pub fn move_selection_with_wrap(&mut self, delta: i32, wrap: bool) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let len = self.items.len() as i32;
        let next = self.selected as i32 + delta;
        let prev = self.selected;
        if wrap {
            self.selected = next.rem_euclid(len) as usize;
        } else if next < 0 {
            return false;
        } else {
            self.selected = next.min(len - 1) as usize;
        }
        if self.selected != prev {
            self.has_changed_index = true;
            self.refresh_common_prefix();
        }
        true
    }

    pub fn selected_item(&self) -> Option<&crate::list::SuggestionItem> {
        self.items.get(self.selected)
    }

    pub fn change_size(&mut self, increase: bool) {
        if increase {
            self.size_scale *= 1.1;
        } else {
            self.size_scale = (self.size_scale / 1.1).max(0.5);
        }
    }

    pub fn start_shake(&mut self, cx: &mut gpui::Context<'_, Self>) {
        self.shaking = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            this.update(cx, |overlay, cx| {
                overlay.shaking = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Default for OverlayState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::list::SuggestionItem;

    #[test]
    fn hide_keeps_items_and_esc_suppresses_auto_show() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        overlay.hide();
        assert!(!overlay.visible);
        assert_eq!(overlay.items.len(), 1);
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "commit".into(),
                ..SuggestionItem::default()
            }],
            "co".into(),
        );
        assert!(overlay.visible);

        overlay.hide_until_shown();
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        assert!(!overlay.visible);
        assert_eq!(overlay.items[0].name, "checkout");
    }

    #[test]
    fn dismiss_clears_items() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        overlay.dismiss();
        assert!(!overlay.visible);
        assert!(overlay.items.is_empty());
        assert!(overlay.search_term.is_empty());
    }

    #[test]
    fn debug_window_survives_hide_and_dismiss() {
        let mut overlay = OverlayState::new();
        overlay.debug_window = true;
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        overlay.hide();
        assert!(overlay.debug_window);
        overlay.dismiss();
        assert!(
            overlay.debug_window,
            "`ec debug autocomplete-window` is a process flag, not per-result state"
        );
    }

    fn two_items() -> OverlayState {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        overlay
    }

    fn subcommand(name: &str) -> SuggestionItem {
        SuggestionItem {
            name: name.into(),
            kind: "subcommand".into(),
            ..SuggestionItem::default()
        }
    }

    #[test]
    fn set_suggestions_and_selection_refresh_the_cached_common_prefix() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(vec![subcommand("checkout"), subcommand("cherry-pick")], "ch".into());
        assert_eq!(&*overlay.common_prefix, "che");
        overlay.move_selection_with_wrap(1, false);
        assert_eq!(&*overlay.common_prefix, "che");
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "--help".into(),
                    kind: "option".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "--hard".into(),
                    kind: "option".into(),
                    ..SuggestionItem::default()
                },
            ],
            "--h".into(),
        );
        assert_eq!(&*overlay.common_prefix, "--h");
        overlay.dismiss();
        assert_eq!(&*overlay.common_prefix, "");
    }

    #[test]
    fn down_past_last_item_stays_on_last() {
        let mut overlay = two_items();
        overlay.selected = 1;
        assert!(overlay.move_selection_with_wrap(1, false));
        assert_eq!(overlay.selected, 1);
    }

    #[test]
    fn up_from_first_item_leaves_the_list() {
        let mut overlay = two_items();
        assert!(!overlay.move_selection_with_wrap(-1, false));
        assert_eq!(overlay.selected, 0);
    }

    #[test]
    fn wrap_around_moves_from_last_to_first() {
        let mut overlay = two_items();
        overlay.selected = 1;
        assert!(overlay.move_selection_with_wrap(1, true));
        assert_eq!(overlay.selected, 0);
    }

    #[test]
    fn set_suggestions_keeps_the_manually_selected_row() {
        let mut overlay = two_items();
        overlay.move_selection_with_wrap(1, false);
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert_eq!(overlay.items[overlay.selected].name, "commit");
        assert!(overlay.has_changed_index);
    }

    #[test]
    fn set_suggestions_resets_when_the_selected_row_disappears() {
        let mut overlay = two_items();
        overlay.move_selection_with_wrap(1, false);
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "ch".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert!(!overlay.has_changed_index);
    }

    #[test]
    fn set_suggestions_resets_when_the_user_has_not_moved() {
        let mut overlay = two_items();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert_eq!(overlay.items[0].name, "commit");
    }

    #[test]
    fn set_suggestions_does_not_keep_selection_while_hidden() {
        let mut overlay = two_items();
        overlay.move_selection_with_wrap(1, false);
        overlay.hide_until_shown();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "commit".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "checkout".into(),
                    ..SuggestionItem::default()
                },
            ],
            "c".into(),
        );
        assert!(!overlay.visible);
        assert_eq!(overlay.selected, 0);
        assert!(!overlay.has_changed_index);
    }

    #[test]
    fn current_arg_keeps_description_only_overlay_visible() {
        let mut overlay = OverlayState::new();
        overlay.set_current_arg("branch", "The branch name");
        overlay.set_suggestions(Vec::new(), "".into());
        assert!(overlay.visible);
        assert!(overlay.has_current_arg());
        assert_eq!(overlay.current_arg_rows(), 2);
    }

    #[test]
    fn keeps_raw_search_for_deletion_and_normalized_match_term_for_highlighting() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions_with_match_term(
            vec![SuggestionItem {
                name: "checkout".into(),
                ..SuggestionItem::default()
            }],
            "'che".into(),
            "che".into(),
        );
        assert_eq!(overlay.search_term, "'che");
        assert_eq!(overlay.match_term, "che");
    }

    #[test]
    fn selection_identity_does_not_keep_same_name_with_changed_insert_metadata() {
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(
            vec![
                SuggestionItem {
                    name: "alias".into(),
                    kind: "option".into(),
                    insert_value: Some("--first".into()),
                    description: "first".into(),
                    ..SuggestionItem::default()
                },
                SuggestionItem {
                    name: "alias".into(),
                    kind: "option".into(),
                    insert_value: Some("--second".into()),
                    description: "second".into(),
                    ..SuggestionItem::default()
                },
            ],
            "a".into(),
        );
        overlay.move_selection_with_wrap(1, false);
        overlay.set_suggestions(
            vec![SuggestionItem {
                name: "alias".into(),
                kind: "option".into(),
                insert_value: Some("--first".into()),
                description: "first".into(),
                ..SuggestionItem::default()
            }],
            "a".into(),
        );
        assert_eq!(overlay.selected, 0);
        assert!(!overlay.has_changed_index);
    }

    #[test]
    fn selection_identity_ignores_presentation_and_query_term_changes() {
        let mut first = SuggestionItem {
            name: "alias".into(),
            kind: "arg".into(),
            display_name: Some("Before".into()),
            query_term: Some("before".into()),
            ..SuggestionItem::default()
        };
        let first_identity = crate::list::selection_identity(&first);
        first.display_name = Some("After".into());
        first.query_term = Some("after".into());
        assert_eq!(first_identity, crate::list::selection_identity(&first));
    }

    #[test]
    fn insertion_suppression_is_one_shot_and_separate_from_hide_until_shown() {
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_next_completion();
        assert!(overlay.take_suppress_completion_for("git status", 10));
        assert!(!overlay.take_suppress_completion_for("git status", 10));
        overlay.hide_until_shown();
        assert!(overlay.suppress_until_shown);
    }

    #[test]
    fn unchanged_insertion_suppression_matches_only_the_acknowledged_buffer() {
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_unchanged_completion("git add .".into(), 9);
        assert!(overlay.take_suppress_completion_for("git add .", 9));
        assert!(!overlay.take_suppress_completion_for("git add .", 9));

        overlay.mark_suppress_unchanged_completion("git add .".into(), 9);
        assert!(!overlay.take_suppress_completion_for("git add ./", 10));
        assert!(!overlay.take_suppress_completion_for("git add .", 9));
    }

    #[test]
    fn layout_flags_report_only_real_changes() {
        let mut overlay = OverlayState::new();
        assert!(!overlay.set_layout_flags(false, false));
        assert!(overlay.set_layout_flags(true, false));
        assert!(!overlay.set_layout_flags(true, false));
        assert!(overlay.set_layout_flags(true, true));
    }

    #[test]
    fn temporary_history_mode_does_not_resize_the_list() {
        let mut overlay = OverlayState::new();
        overlay.list_width = 320.0;
        assert_eq!(overlay.effective_list_width(), 320.0);
        overlay.history_mode = true;
        assert_eq!(overlay.effective_list_width(), 320.0);
    }
}

pub type OverlayHandle = WindowHandle<SuggestionList>;

pub fn overlay_window_options(bounds: Bounds<Pixels>, show: bool) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: false,
        show,
        kind: gpui::WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: gpui::WindowBackgroundAppearance::Transparent,
        window_min_size: Some(size(px(1.), px(1.))),
        app_id: Some("easy-complete".into()),
        ..Default::default()
    }
}

#[cfg(test)]
mod window_option_tests {
    use super::*;

    #[test]
    fn overlay_window_is_a_non_activating_popup() {
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(32.), px(32.)),
        };
        let hidden = overlay_window_options(bounds, false);
        assert!(!hidden.focus, "overlay must not steal terminal focus");
        assert!(!hidden.show);
        assert!(matches!(hidden.kind, gpui::WindowKind::PopUp));
        let shown = overlay_window_options(bounds, true);
        assert!(!shown.focus);
        assert!(shown.show);
        assert!(matches!(shown.kind, gpui::WindowKind::PopUp));
    }

    #[test]
    fn overlay_window_title_is_easy_complete() {
        assert_eq!(OVERLAY_WINDOW_TITLE, "Easy Complete");
        assert!(
            !OVERLAY_WINDOW_TITLE.contains("Fig"),
            "overlay title is a product string, not the Fig fork name"
        );
    }
}

pub fn open_overlay_window(cx: &mut App, state: Entity<OverlayState>) -> anyhow::Result<OverlayHandle> {
    // Create shown so AppKit attaches an `NSWindow`, then `orderOut` like the old
    // tao `set_visible(false)` — keep the last size, do not shrink to 1×1.
    let handle = open_overlay_window_with_visibility(cx, state, true)?;
    park_overlay_handle(&handle, cx)?;
    Ok(handle)
}

pub fn open_overlay_window_with_visibility(
    cx: &mut App,
    state: Entity<OverlayState>,
    show: bool,
) -> anyhow::Result<OverlayHandle> {
    let bounds = Bounds {
        origin: point(px(-10_000.), px(-10_000.)),
        size: size(px(DEFAULT_WIDTH as f32), px(DEFAULT_MAX_LIST_HEIGHT as f32)),
    };
    let handle = cx.open_window(overlay_window_options(bounds, show), |_, cx| {
        cx.new(|_cx| SuggestionList::new(state))
    })?;
    handle
        .update(cx, |_list, window, _cx| {
            window.set_window_title(OVERLAY_WINDOW_TITLE);
            harden_overlay_window_handle(window);
            if !show {
                park_overlay_window_handle(window);
            }
        })
        .map(|_| ())?;
    Ok(handle)
}

pub fn park_overlay_handle(handle: &OverlayHandle, cx: &mut App) -> anyhow::Result<()> {
    handle
        .update(cx, |list, window, _cx| {
            // A hidden window may be shown again at the same bounds. Do not
            // let the previous frame/size cache suppress that first request:
            // the native window is currently ordered out and needs a fresh
            // orderFront path.
            list.last_requested_size = None;
            list.last_requested_frame = None;
            park_overlay_window_handle(window);
        })
        .map(|_| ())
}

fn requested_frames_close(previous: Option<(f32, f32, f32, f32)>, next: (f32, f32, f32, f32)) -> bool {
    const EPS: f32 = 0.5;
    previous.is_some_and(|previous| {
        (previous.0 - next.0).abs() < EPS
            && (previous.1 - next.1).abs() < EPS
            && (previous.2 - next.2).abs() < EPS
            && (previous.3 - next.3).abs() < EPS
    })
}

pub fn position_overlay(
    origin: Point<Pixels>,
    size: Size<Pixels>,
    handle: &OverlayHandle,
    cx: &mut App,
) -> anyhow::Result<()> {
    let applied = handle.update(cx, |list, window, _cx| {
        // GPUI owns native resize (including DPI). We only pin origin/show below.
        let requested = (f32::from(size.width), f32::from(size.height));
        // A native resize callback can be rejected while GPUI's AppCell is
        // already borrowed. Do not let our request cache turn that one
        // missed callback into a permanently stale renderer: an unchanged
        // requested size is retried whenever the live viewport disagrees.
        if list.last_requested_size != Some(requested) || window.viewport_size() != size {
            window.resize(size);
        }
        list.last_requested_size = Some(requested);
        let frame = (
            f32::from(origin.x),
            f32::from(origin.y),
            f32::from(size.width),
            f32::from(size.height),
        );
        if !requested_frames_close(list.last_requested_frame, frame) {
            let ok = set_overlay_frame_handle(window, frame.0 as f64, frame.1 as f64, frame.2 as f64, frame.3 as f64);
            if ok {
                list.last_requested_frame = Some(frame);
            }
            ok
        } else {
            true
        }
    })?;
    if !applied {
        anyhow::bail!("native overlay frame was not applied");
    }
    Ok(())
}
