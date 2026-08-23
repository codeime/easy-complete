//! GPUI overlay controller: suggestion list, caret placement, key actions, engine.

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ec_engine::{CompleteRequest, CompleteResult, EngineClient, ranking_root_command, ui_completion_deadline};
use ec_gpui::{
    ClickInsert, DEFAULT_FONT_SIZE, DEFAULT_MAX_LIST_HEIGHT, DEFAULT_WIDTH, OverlayHandle, OverlayState, OverlayTheme,
    SuggestionItem, TabPrefix, open_overlay_window, overlay_content_size_with_context, overlay_screens,
    park_overlay_handle, position_overlay, tab_prefix_insertion, theme_from_json,
};
use fig_proto::figterm::Action;
use fig_proto::local::caret_position_hook::Origin;
use fig_remote_ipc::figterm::{FigtermCommand, FigtermState, InterceptMode};
use fig_settings::keybindings::{KeyBinding, KeyBindings};
use gpui::{App, AppContext as _, Entity, Pixels, Point, Size, px, size};
use tao::dpi::{LogicalPosition, LogicalSize, Position};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::event::{Event, WindowPosition};
use crate::event_loop::EventLoopProxy;
use crate::platform::PlatformState;

/// Overlay is always on. `EC_GPUI_OVERLAY=0` remains an emergency kill switch.
pub fn gpui_overlay_enabled() -> bool {
    if let Ok(val) = std::env::var("EC_GPUI_OVERLAY") {
        let val = val.to_ascii_lowercase();
        return val != "0" && val != "false" && val != "off";
    }
    true
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LastInput {
    buffer: String,
    cwd: String,
    cursor: u32,
    session_id: Uuid,
}

pub struct OverlayController {
    pub state: Entity<OverlayState>,
    handle: Arc<Mutex<Option<OverlayHandle>>>,
    engine: EngineClient,
    session_id: Arc<Mutex<Option<Uuid>>>,
    generation: Arc<AtomicU64>,
    /// Generation that turned the `···` marker on, or `NO_LOADING_OWNER`.
    ///
    /// The legacy WebView derived its loading flag from the set of in-flight
    /// generators, so it could never be left on with nothing running. Here the
    /// marker is a latch driven by two separate events, so it needs an explicit
    /// owner: whichever request switched it on must also switch it off, even
    /// when its result arrives too late to be displayed.
    loading_owner: Arc<AtomicU64>,
    enabled: bool,
    figterm_state: Arc<FigtermState>,
    platform_state: Arc<PlatformState>,
    last_position: Arc<Mutex<Option<WindowPosition>>>,
    last_input: Arc<Mutex<Option<LastInput>>>,
    /// Buffer we expect the shell to report back after our own insertion.
    /// Stands in for the legacy `justInserted` flag, which kept the paste
    /// heuristic from mistaking an accepted completion for a paste.
    self_insertion: Arc<Mutex<Option<String>>>,
    proxy: EventLoopProxy,
}

impl OverlayController {
    pub fn start(
        cx: &mut App,
        engine: EngineClient,
        proxy: EventLoopProxy,
        figterm_state: Arc<FigtermState>,
        platform_state: Arc<PlatformState>,
    ) -> anyhow::Result<Self> {
        let session_id = Arc::new(Mutex::new(None));
        let state = cx.new(|_| {
            let mut overlay = OverlayState::new();
            apply_settings(&mut overlay);
            overlay
        });
        let click_session = session_id.clone();
        let click_generation = Arc::new(AtomicU64::new(0));
        let generation = click_generation.clone();
        let click_proxy = proxy.clone();
        let disable_session = session_id.clone();
        let disable_proxy = proxy.clone();
        state.update(cx, |overlay, _cx| {
            overlay.on_click_insert = Some(Arc::new(move |click| {
                let Some(session_id) = *click_session.lock().unwrap_or_else(|err| err.into_inner()) else {
                    return;
                };
                let _ = click_proxy.send_event(Event::AutocompleteClick {
                    click,
                    session_id,
                    generation: click_generation.load(Ordering::Relaxed),
                });
            }));
            overlay.on_disable_dev_mode = Some(Arc::new(move || {
                let _ = fig_settings::settings::set_value("autocomplete.developerModeNPM", false);
                let Some(session_id) = *disable_session.lock().unwrap_or_else(|err| err.into_inner()) else {
                    return;
                };
                let _ = disable_proxy.send_event(Event::AutocompleteAction {
                    action: "relayoutOverlay".into(),
                    session_id,
                });
            }));
        });
        let handle = Arc::new(Mutex::new(None));
        Ok(Self {
            state,
            handle,
            engine,
            session_id,
            generation,
            loading_owner: Arc::new(AtomicU64::new(NO_LOADING_OWNER)),
            enabled: gpui_overlay_enabled()
                && !fig_settings::settings::get_bool_or("autocomplete.disable", false)
                && PlatformState::accessibility_is_enabled().unwrap_or(true),
            figterm_state,
            platform_state,
            last_position: Arc::new(Mutex::new(None)),
            last_input: Arc::new(Mutex::new(None)),
            self_insertion: Arc::new(Mutex::new(None)),
            proxy,
        })
    }

    fn ensure_window(&mut self, cx: &mut App) -> Option<OverlayHandle> {
        ensure_overlay_window(&self.handle, &self.state, cx)
    }

    fn park_window(&self, cx: &mut App) {
        let _ = park_overlay_slot(&self.handle, cx);
    }

    fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn take_loading_owner(&self) -> Option<u64> {
        match self.loading_owner.swap(NO_LOADING_OWNER, Ordering::Relaxed) {
            NO_LOADING_OWNER => None,
            owner => Some(owner),
        }
    }

    /// Drop the `···` marker while keeping any rows that were already on
    /// screen. With nothing to show, the window is parked instead of being left
    /// as an empty card over the terminal.
    fn clear_stale_loading(&mut self, cx: &mut App) {
        let park = self.state.update(cx, |overlay, cx| {
            if !overlay.loading {
                return false;
            }
            overlay.loading = false;
            let empty = overlay.items.is_empty();
            if empty {
                overlay.visible = false;
            }
            cx.notify();
            empty
        });
        if park {
            self.park_window(cx);
        }
    }

    /// Turn the marker off unless a newer request already owns it. Returns
    /// `true` when this call released the latch.
    fn release_loading_owner(&self, generation: u64) -> bool {
        let owner = self.loading_owner.load(Ordering::Relaxed);
        if !loading_owner_is_released_by(owner, generation) {
            return false;
        }
        self.loading_owner
            .compare_exchange(owner, NO_LOADING_OWNER, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    fn current_session(&self) -> Option<Uuid> {
        *self.session_id.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn forget_last_input(&self) {
        *self.last_input.lock().unwrap_or_else(|err| err.into_inner()) = None;
    }

    fn take_self_insertion(&self) -> Option<String> {
        self.self_insertion.lock().unwrap_or_else(|err| err.into_inner()).take()
    }

    /// Record the session that owns the overlay. Returns `true` only for a
    /// switch between two real sessions (not the initial assignment).
    fn set_session(&self, session_id: Uuid) -> bool {
        let (changed, switched) = {
            let mut current = self.session_id.lock().unwrap_or_else(|err| err.into_inner());
            let switched = current.is_some() && *current != Some(session_id);
            let changed = session_changed(*current, session_id);
            *current = Some(session_id);
            (changed, switched)
        };
        if changed {
            // A completion from the previous terminal must not repaint this
            // overlay after a session switch. The caller also clears UI state
            // and the old caret when this is a real session-to-session switch.
            self.bump_generation();
            let mut last_input = self.last_input.lock().unwrap_or_else(|err| err.into_inner());
            if last_input.as_ref().is_some_and(|input| input.session_id != session_id) {
                *last_input = None;
            }
        }
        switched
    }

    pub fn apply_theme(&mut self, cx: &mut App) {
        if let Err(err) = fig_settings::settings::init_global() {
            warn!(%err, "failed to reload settings from disk");
        }
        self.state.update(cx, |overlay, cx| {
            apply_settings(overlay);
            cx.notify();
        });
    }

    pub fn set_enabled(&mut self, enabled: bool, cx: &mut App) {
        self.enabled = enabled && gpui_overlay_enabled();
        if !self.enabled {
            self.dismiss(cx);
        }
    }

    /// `ec debug autocomplete-window`. Paints the overlay window so its
    /// bounds (including transparent padding) are visible. Persists across
    /// hide/dismiss; only this call turns it off.
    pub fn set_debug_mode(&mut self, enabled: bool, cx: &mut App) {
        self.state.update(cx, |overlay, cx| {
            overlay.debug_window = enabled;
            cx.notify();
        });
    }

    pub fn hide(&mut self, cx: &mut App) {
        self.bump_generation();
        self.take_loading_owner();
        self.state.update(cx, |overlay, cx| {
            overlay.hide();
            cx.notify();
        });
        self.park_window(cx);
        self.sync_own_intercept(cx);
    }

    pub fn hide_until_shown(&mut self, cx: &mut App) {
        // Hiding is a cancellation boundary too. Otherwise an in-flight
        // completion can repopulate hidden state and later be shown as stale.
        self.bump_generation();
        self.take_loading_owner();
        self.state.update(cx, |overlay, cx| {
            overlay.hide_until_shown();
            cx.notify();
        });
        self.park_window(cx);
        self.sync_own_intercept(cx);
    }

    pub fn dismiss(&mut self, cx: &mut App) {
        self.bump_generation();
        self.take_loading_owner();
        self.state.update(cx, |overlay, cx| {
            overlay.dismiss();
            cx.notify();
        });
        self.park_window(cx);
        self.sync_own_intercept(cx);
    }

    pub fn show(&mut self, cx: &mut App) {
        let figterm = self.figterm_state.clone();
        self.show_kept_items(&figterm, cx);
    }

    pub fn apply_position(&mut self, position: WindowPosition, platform_state: &PlatformState, cx: &mut App) {
        let screens = overlay_screens();
        #[cfg(not(target_os = "macos"))]
        if matches!(position, WindowPosition::RelativeToCaret { .. }) && screens.is_empty() {
            debug!("no screen list; refusing caret placement");
            *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
            self.park_window(cx);
            self.sync_own_intercept(cx);
            return;
        }
        *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = Some(position);
        let needs_window = {
            let overlay = self.state.read(cx);
            overlay.visible || overlay.loading || overlay.has_current_arg()
        };
        if !needs_window {
            return;
        }
        let Some(handle) = self.ensure_window(cx) else {
            return;
        };
        let positioned = layout_overlay(
            &self.state,
            &self.handle,
            handle,
            &self.last_position,
            platform_state,
            &screens,
            cx,
        );
        self.sync_own_intercept_for_layout(positioned, cx);
    }

    fn relayout(&mut self, cx: &mut App) -> bool {
        let needs_window = {
            let overlay = self.state.read(cx);
            overlay.visible || overlay.loading || overlay.has_current_arg()
        };
        if !needs_window {
            return false;
        }
        let Some(handle) = self.ensure_window(cx) else {
            return false;
        };
        let screens = overlay_screens();
        layout_overlay(
            &self.state,
            &self.handle,
            handle,
            &self.last_position,
            &self.platform_state,
            &screens,
            cx,
        )
    }

    fn sync_own_intercept(&self, cx: &App) {
        self.sync_intercept(&self.figterm_state, cx);
    }

    fn sync_own_intercept_for_layout(&self, positioned: bool, cx: &App) {
        let Some(session_id) = self.current_session() else {
            return;
        };
        let overlay = self.state.read(cx);
        // While the card is showing the loading marker, none of the retained
        // rows is visible. Do not let Enter/Tab accept a stale row from the
        // preceding result behind that marker.
        let has_items = !overlay.loading && !overlay.items.is_empty();
        let has_last_position = self
            .last_position
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some();
        let (visible_intercept, global_intercept) = positioned_intercept_flags(InterceptInputs {
            overlay_visible: overlay.visible,
            has_items,
            positioned,
            has_last_position,
        });
        set_intercept_flags(&self.figterm_state, session_id, visible_intercept, global_intercept);
    }

    fn sync_intercept(&self, figterm_state: &FigtermState, cx: &App) {
        let Some(session_id) = self.current_session() else {
            return;
        };
        let overlay = self.state.read(cx);
        let has_position = self
            .last_position
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some();
        let has_items = !overlay.loading && !overlay.items.is_empty();
        // Not a layout pass: `positioned` is false, so a visible list
        // without a successful place this frame does not swallow keys.
        let (visible_intercept, global_intercept) = positioned_intercept_flags(InterceptInputs {
            overlay_visible: overlay.visible,
            has_items,
            positioned: false,
            has_last_position: has_position,
        });
        set_intercept_flags(figterm_state, session_id, visible_intercept, global_intercept);
    }

    fn show_kept_items(&mut self, figterm_state: &FigtermState, cx: &mut App) {
        let can_show = {
            let overlay = self.state.read(cx);
            !overlay.items.is_empty() || overlay.loading || overlay.has_current_arg()
        };
        if !can_show {
            return;
        }
        if self.ensure_window(cx).is_none() {
            return;
        }
        self.state.update(cx, |overlay, cx| {
            overlay.suppress_until_shown = false;
            overlay.visible = true;
            cx.notify();
        });
        let positioned = self.relayout(cx);
        let Some(session_id) = self.current_session() else {
            return;
        };
        let overlay = self.state.read(cx);
        let has_items = !overlay.loading && !overlay.items.is_empty();
        let (visible_intercept, global_intercept) = positioned_intercept_flags(InterceptInputs {
            overlay_visible: overlay.visible,
            has_items,
            positioned,
            has_last_position: self
                .last_position
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .is_some(),
        });
        set_intercept_flags(figterm_state, session_id, visible_intercept, global_intercept);
    }

    pub fn complete_buffer(
        &mut self,
        buffer: String,
        cwd: String,
        cursor: u32,
        session_id: Uuid,
        figterm_state: Arc<FigtermState>,
        cx: &mut App,
    ) {
        self.complete_buffer_inner(buffer, cwd, cursor, session_id, figterm_state, false, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_buffer_inner(
        &mut self,
        buffer: String,
        cwd: String,
        cursor: u32,
        session_id: Uuid,
        figterm_state: Arc<FigtermState>,
        force: bool,
        cx: &mut App,
    ) {
        let switched_session = self.set_session(session_id);
        if switched_session {
            // Do not expose rows, one-shot insertion state, or a caret from
            // the previously focused terminal while the new request runs.
            self.state.update(cx, |overlay, cx| {
                overlay.dismiss();
                cx.notify();
            });
            *self.last_position.lock().unwrap_or_else(|err| err.into_inner()) = None;
            self.park_window(cx);
            set_intercept(&figterm_state, session_id, false, false);
        }
        let next_input = LastInput {
            buffer: buffer.clone(),
            cwd: cwd.clone(),
            cursor,
            session_id,
        };
        let (duplicate_input, previous_input) = {
            let mut last_input = self.last_input.lock().unwrap_or_else(|err| err.into_inner());
            let duplicate = should_skip_duplicate_input(last_input.as_ref(), &next_input, force);
            (duplicate, last_input.replace(next_input))
        };
        let previous_buffer = previous_input
            .filter(|input| input.session_id == session_id)
            .map(|input| input.buffer);
        let self_inserted = self.take_self_insertion().is_some_and(|predicted| predicted == buffer);
        if !self.enabled {
            #[cfg(target_os = "macos")]
            {
                // AX grant can land while the cached `enabled` flag is still false (notification
                // lag). Re-read the live TCC flag so the first keystroke after granting works.
                if macos_utils::accessibility::accessibility_is_enabled()
                    && gpui_overlay_enabled()
                    && !fig_settings::settings::get_bool_or("autocomplete.disable", false)
                {
                    self.enabled = true;
                }
            }
        }
        if !self.enabled || buffer.trim().is_empty() || cursor_is_inside_word(&buffer, cursor) {
            self.dismiss(cx);
            set_intercept(&figterm_state, session_id, false, false);
            return;
        }

        // Accepting a completion produces one shell buffer notification. The
        // legacy WebView kept itself in HIDDEN_BY_INSERTION for that update so
        // the just-accepted row did not immediately reappear. Consume this
        // one-shot marker before starting another engine request; the next
        // real keypress is then free to show fresh suggestions.
        let suppress_insert_update = self.state.update(cx, |overlay, cx| {
            let suppress = overlay.take_suppress_completion_for(&buffer, cursor);
            if suppress {
                overlay.hide();
                cx.notify();
            }
            suppress
        });
        if suppress_insert_update {
            self.park_window(cx);
            self.sync_intercept(&figterm_state, cx);
            return;
        }

        // Terminal hooks can publish the same edit buffer repeatedly while a
        // command writes output. Invalidating and restarting the in-flight
        // completion each time can otherwise keep the UI in loading forever.
        // Explicit recomputation (history/fuzzy/settings changes) bypasses
        // this guard through `force`.
        if duplicate_input {
            return;
        }

        // Two legacy `HIDDEN_UNTIL_KEYPRESS` triggers. Backspacing a whole
        // token away should not pop the previous token's full option list back
        // up, and a paste or a history recall is not a request for
        // suggestions. Either way the next real keystroke shows the list again.
        if let Some(previous) = previous_buffer.as_deref()
            && (backspaced_to_new_token(previous, &buffer)
                || (!self_inserted && large_buffer_change(previous, &buffer)))
        {
            self.hide(cx);
            self.sync_intercept(&figterm_state, cx);
            return;
        }

        if self.state.read(cx).only_show_on_tab {
            let new_token = buffer.chars().last().is_some_and(char::is_whitespace);
            let hidden = !self.state.read(cx).visible;
            if hidden || new_token {
                self.hide_until_shown(cx);
                self.sync_intercept(&figterm_state, cx);
            }
        }

        let generation = self.bump_generation();
        self.take_loading_owner();
        self.clear_stale_loading(cx);
        let (fuzzy, history_only, include_history, suggest_first_token) = {
            let overlay = self.state.read(cx);
            let history_only = overlay.history_mode || overlay.history_setting == "history_only";
            (
                overlay.fuzzy_search,
                history_only,
                history_only || overlay.history_setting != "off",
                overlay.first_token_completion,
            )
        };
        let (current_shell, current_process, environment_variables) = figterm_state
            .with(&session_id, |session| {
                let context = session.context.as_ref();
                (
                    context.and_then(|context| context.shell_path.clone()),
                    context.and_then(|context| context.process_name.clone()),
                    session.flattened_env.clone(),
                )
            })
            .unwrap_or_else(|| (None, None, Arc::new(Vec::new())));
        let request = CompleteRequest {
            buffer,
            cwd: cwd.clone(),
            cursor: Some(cursor),
            fuzzy,
            history_only,
            include_history,
            suggest_first_token,
            current_shell,
            current_process,
            environment_variables,
        };
        let engine = self.engine.clone();
        let proxy = self.proxy.clone();
        let executor = cx.background_executor().clone();

        cx.spawn(async move |_cx| {
            let complete = engine.complete(request);
            futures::pin_mut!(complete);
            let timed = executor.timer(Duration::from_millis(200));
            futures::pin_mut!(timed);
            let result = match futures::future::select(complete, timed).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right((_, complete)) => {
                    let _ = proxy.send_event(Event::GpuiOverlayLoading { generation });
                    // The user's own script budget bounds how long `···` is
                    // worth showing, but not the request: the engine already
                    // has a supervisor watchdog, and a result that lands after
                    // the marker is retired is still the one the legacy
                    // WebView would eventually have rendered.
                    let deadline = executor.timer(ui_completion_deadline());
                    futures::pin_mut!(deadline);
                    match futures::future::select(complete, deadline).await {
                        futures::future::Either::Left((result, _)) => result,
                        futures::future::Either::Right((_, complete)) => {
                            let _ = proxy.send_event(Event::GpuiOverlayLoadingExpired { generation });
                            complete.await
                        },
                    }
                },
            };
            let _ = proxy.send_event(Event::GpuiOverlayComplete {
                generation,
                result,
                session_id,
                cwd,
            });
        })
        .detach();
    }

    /// The `···` marker outlived the user's script budget. Retire it and park
    /// an empty card, but leave the request running: the engine watchdog still
    /// bounds it, and a late result is better than none at all.
    pub fn expire_loading(&mut self, generation: u64, cx: &mut App) {
        if self.release_loading_owner(generation) {
            self.clear_stale_loading(cx);
        }
    }

    pub fn show_loading(&mut self, generation: u64, cx: &mut App) {
        if self.generation.load(Ordering::Relaxed) != generation {
            return;
        }
        self.loading_owner.store(generation, Ordering::Relaxed);
        let visible = self.state.update(cx, |overlay, cx| {
            overlay.loading = true;
            if !overlay.suppress_until_shown {
                overlay.visible = true;
            }
            cx.notify();
            overlay.visible
        });
        if !visible {
            return;
        }
        let positioned = self.ensure_window(cx).is_some_and(|handle| {
            let screens = overlay_screens();
            layout_overlay(
                &self.state,
                &self.handle,
                handle,
                &self.last_position,
                &self.platform_state,
                &screens,
                cx,
            )
        });
        self.sync_own_intercept_for_layout(positioned, cx);
    }

    pub fn apply_completion(
        &mut self,
        generation: u64,
        result: anyhow::Result<CompleteResult>,
        session_id: Uuid,
        cwd: &str,
        cx: &mut App,
    ) {
        // Release the latch before the currency check. A superseded result is
        // not worth rendering, but it is still the only signal that the request
        // which turned the marker on has finished — dropping it here is what
        // used to strand `···` on screen until the next keystroke.
        let released = self.release_loading_owner(generation);
        if !completion_is_current(
            generation,
            self.generation.load(Ordering::Relaxed),
            self.current_session(),
            session_id,
        ) {
            if released {
                self.clear_stale_loading(cx);
            }
            return;
        }
        if result.is_err() {
            // A failed attempt produced nothing to show, so the recorded input
            // must not keep the duplicate guard armed: the terminal
            // republishing the same buffer is then the only path back to a
            // suggestion. Superseded results never reach here, so this cannot
            // reopen the storm the guard exists to stop.
            self.forget_last_input();
        }
        apply_complete_result(
            self.state.clone(),
            &self.handle,
            result,
            session_id,
            &self.figterm_state,
            cwd,
            &self.last_position,
            &self.platform_state,
            cx,
        );
    }

    /// Re-run the current buffer even when its text is unchanged. Settings
    /// changes (notably the auto-execute visibility toggle) must update the
    /// retained rows immediately instead of waiting for another keystroke.
    /// Fire-and-forget: the supervisor clears caches before the next complete.
    /// Do not block the GPUI thread waiting on the engine worker.
    pub fn clear_engine_caches(&self) {
        let engine = self.engine.clone();
        tokio::spawn(async move {
            if let Err(err) = engine.clear_caches().await {
                error!(%err, "failed to clear autocomplete caches");
            }
        });
    }

    pub fn recomplete(&mut self, cx: &mut App) {
        let Some(current_session) = self.current_session() else {
            return;
        };
        let Some(input) = self.last_input.lock().unwrap_or_else(|err| err.into_inner()).clone() else {
            return;
        };
        if input.session_id != current_session {
            return;
        }
        self.complete_buffer_inner(
            input.buffer,
            input.cwd,
            input.cursor,
            input.session_id,
            self.figterm_state.clone(),
            true,
            cx,
        );
    }

    pub fn handle_action(&mut self, action: &str, action_session_id: Uuid, figterm_state: &FigtermState, cx: &mut App) {
        if !action_session_is_current(self.current_session(), action_session_id) {
            debug!(%action_session_id, current_session = ?self.current_session(), action, "ignoring stale overlay action");
            return;
        }
        let (visible, loading) = {
            let overlay = self.state.read(cx);
            (overlay.visible, overlay.loading)
        };
        if !action_is_allowed(action, visible, loading) {
            debug!(action, loading, "ignoring overlay action without an actionable list");
            return;
        }
        match action {
            "navigateUp" => self.move_selection(-1, true, figterm_state, cx),
            "navigateDown" => self.move_selection(1, false, figterm_state, cx),
            "hideAutocomplete" => {
                self.hide_until_shown(cx);
                self.sync_intercept(figterm_state, cx);
            },
            // `showAutocomplete` is an explicit show action.  It must only
            // reveal the kept rows; accepting the sole row is reserved for
            // the hidden-overlay Tab shortcut below.
            "showAutocomplete" => self.show_kept_items(figterm_state, cx),
            "showAutocompleteFromTab" => {
                if self.state.read(cx).items.len() == 1 {
                    self.insert_selected(false, figterm_state, cx);
                } else {
                    self.show_kept_items(figterm_state, cx);
                }
            },
            "toggleAutocomplete" => {
                let visible = self.state.read(cx).visible;
                if visible {
                    self.hide_until_shown(cx);
                    self.sync_intercept(figterm_state, cx);
                } else {
                    self.show_kept_items(figterm_state, cx);
                }
            },
            "relayoutOverlay" => {
                self.relayout(cx);
            },
            "insertSelected" => self.insert_selected(false, figterm_state, cx),
            "insertCommonPrefixOrInsertSelected" => {
                if !self.insert_common_prefix(figterm_state, cx) {
                    self.insert_selected(false, figterm_state, cx);
                }
            },
            "insertSelectedAndExecute" => self.insert_selected(true, figterm_state, cx),
            "execute" => {
                let _ = self.insert_text("\n", 0, true, figterm_state, cx);
            },
            "insertCommonPrefix" => {
                if !self.insert_common_prefix(figterm_state, cx) {
                    self.shake(cx);
                }
            },
            "insertCommonPrefixOrNavigateDown" => {
                if !self.insert_common_prefix(figterm_state, cx) {
                    self.move_selection(1, false, figterm_state, cx);
                }
            },
            "toggleDescription" => {
                if !fig_settings::settings::get_bool_or("autocomplete.alwaysShowDescription", false) {
                    self.state.update(cx, |overlay, cx| {
                        overlay.description_popout = !overlay.description_popout;
                        if !overlay.description_popout {
                            overlay.is_above_cursor = false;
                        }
                        cx.notify();
                    });
                    self.relayout(cx);
                }
            },
            "showDescription" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.description_popout = true;
                    cx.notify();
                });
                self.relayout(cx);
            },
            "hideDescription" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.description_popout = false;
                    overlay.is_above_cursor = false;
                    cx.notify();
                });
                self.relayout(cx);
            },
            "toggleHistoryMode" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.history_mode = !overlay.history_mode;
                    cx.notify();
                });
                self.recomplete(cx);
            },
            "toggleFuzzySearch" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.fuzzy_search = !overlay.fuzzy_search;
                    overlay.effective_fuzzy_search = overlay.fuzzy_search;
                    cx.notify();
                });
                // The keybinding is a temporary overlay-mode toggle in the
                // WebView. Only the settings UI persists the user default.
                self.recomplete(cx);
            },
            "increaseSize" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.change_size(true);
                    cx.notify();
                });
                self.relayout(cx);
            },
            "decreaseSize" => {
                self.state.update(cx, |overlay, cx| {
                    overlay.change_size(false);
                    cx.notify();
                });
                self.relayout(cx);
            },
            other if other.starts_with("selectSuggestion") => {
                if let Ok(n) = other.trim_start_matches("selectSuggestion").parse::<usize>() {
                    if let Some(index) = select_suggestion_index(n, self.state.read(cx).items.len()) {
                        self.state.update(cx, |overlay, cx| {
                            overlay.selected = index;
                            overlay.has_changed_index = true;
                            cx.notify();
                        });
                    }
                }
            },
            other => debug!(action = other, "unhandled overlay action"),
        }
    }

    pub fn handle_click(
        &mut self,
        click: ClickInsert,
        click_session_id: Uuid,
        click_generation: u64,
        figterm_state: &FigtermState,
        cx: &mut App,
    ) {
        let click_is_current = {
            let overlay = self.state.read(cx);
            click_matches_current(&click, overlay)
        };
        if !completion_is_current(
            click_generation,
            self.generation.load(Ordering::Relaxed),
            self.current_session(),
            click_session_id,
        ) || !click_is_current
        {
            debug!(
                %click_session_id,
                current_session = ?self.current_session(),
                click_generation,
                current_generation = self.generation.load(Ordering::Relaxed),
                "ignoring stale overlay click"
            );
            return;
        }
        self.insert_item(click, false, figterm_state, cx);
    }

    fn shake(&self, cx: &mut App) {
        self.state.update(cx, |overlay, cx| {
            overlay.start_shake(cx);
        });
    }

    fn move_selection(&mut self, delta: i32, up_from_top: bool, figterm_state: &FigtermState, cx: &mut App) {
        let wrap = self.state.read(cx).scroll_wrap_around;
        let at_top = self.state.read(cx).selected == 0;
        let still_visible = self.state.update(cx, |overlay, cx| {
            let visible = overlay.move_selection_with_wrap(delta, wrap);
            cx.notify();
            visible
        });
        if !still_visible {
            if up_from_top && at_top && self.state.read(cx).navigate_to_history {
                self.state.update(cx, |overlay, cx| {
                    overlay.history_mode = !overlay.history_mode;
                    cx.notify();
                });
                self.recomplete(cx);
                return;
            }
            // Up-from-top maps to the legacy HIDDEN_UNTIL_KEYPRESS state:
            // cancel the current generation, but allow the next real buffer
            // change to show suggestions again. `hide_until_shown` is the
            // stronger Esc/onlyShowOnTab state and would keep the list hidden.
            self.hide(cx);
            self.sync_intercept(figterm_state, cx);
        }
    }

    fn insert_common_prefix(&mut self, figterm_state: &FigtermState, cx: &mut App) -> bool {
        let decision = {
            let overlay = self.state.read(cx);
            tab_prefix_insertion(overlay.selected, &overlay.items, &overlay.search_term)
        };
        match decision {
            Some(TabPrefix::Full(_)) => {
                self.insert_selected(false, figterm_state, cx);
                true
            },
            Some(TabPrefix::Partial(shared)) => {
                let (search, completes_the_row) = {
                    let overlay = self.state.read(cx);
                    let Some(item) = overlay.selected_item() else {
                        return false;
                    };
                    (
                        item.query_term.clone().unwrap_or_else(|| overlay.search_term.clone()),
                        prefix_completes_row(&shared, item),
                    )
                };
                // The shared prefix can already spell out the whole selected
                // row. Fig treated that as a real acceptance so the trailing
                // space, separator and post-insert hide all still happen;
                // inserting it as a bare prefix would strand the caret.
                if completes_the_row {
                    self.insert_selected(false, figterm_state, cx);
                    return true;
                }
                let shared = shared.replace(' ', r"\ ");
                let (insertion, deletion) = insertion_for(&shared, &search);
                if insertion.is_empty() && deletion == 0 {
                    return false;
                }
                self.insert_text(&insertion, deletion, false, figterm_state, cx);
                true
            },
            None => false,
        }
    }

    fn insert_selected(&mut self, execute: bool, figterm_state: &FigtermState, cx: &mut App) {
        let selected = {
            let overlay = self.state.read(cx);
            let Some(item) = overlay.selected_item() else {
                return;
            };
            ClickInsert {
                name: item.name.clone(),
                description: item.description.clone(),
                search: overlay.search_term.clone(),
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
        };
        self.insert_item(selected, execute, figterm_state, cx);
    }

    fn insert_item(&mut self, item: ClickInsert, execute: bool, figterm_state: &FigtermState, cx: &mut App) {
        let input_before_accept = self.current_input_snapshot();
        let acceptance = accepted_suggestion_key(&item, input_before_accept.as_ref());
        let add_space = self.state.read(cx).insert_space_automatically;
        let text = full_insertion_for_item(
            &item.name,
            item.insert_value.as_deref(),
            &item.kind,
            item.separator_to_add.as_deref(),
            item.should_add_space,
            add_space,
            execute,
        );
        let search_term = insertion_search_term(&item.kind, &item.search, item.query_term.as_deref()).to_string();
        let kind = item.kind;
        let opens_new_arg = opens_new_arg(add_space, item.should_add_space, item.separator_to_add.as_deref());
        let text = resolve_cursor_marker(text);
        // Auto-execute/special rows represent an action on the existing
        // buffer, not a replacement for the current query.  The old WebView
        // therefore sent `\n` with zero deletion; blindly using insertion_for
        // here would backspace `status` before executing it.
        let (insertion, deletion) = insertion_for_kind(&text, &search_term, &kind);
        let should_suppress = should_suppress_after_insert(execute, &kind, opens_new_arg, &text);
        let inserted = self.insert_text(&insertion, deletion, execute, figterm_state, cx);
        if inserted
            && let Some((root_command, accepted_name)) = acceptance
            && let Err(err) = self.engine.record_acceptance(root_command, accepted_name)
        {
            debug!(%err, "failed to record autocomplete acceptance");
        }
        let changed_buffer = insertion_changes_buffer(&insertion, deletion, execute);
        if inserted && changed_buffer && should_suppress {
            if let Some((expected_buffer, expected_cursor)) = input_before_accept
                .as_ref()
                .and_then(|(buffer, cursor)| predicted_buffer_after_insert(buffer, *cursor, &insertion, deletion))
            {
                self.state.update(cx, |overlay, cx| {
                    // Bind suppression to the exact post-accept edit-buffer
                    // notification. A bare one-shot flag could consume the
                    // user's next real keypress when the PTY drops the ack.
                    overlay.mark_suppress_unchanged_completion(expected_buffer, expected_cursor);
                    cx.notify();
                });
            }
        } else if inserted && !changed_buffer {
            if let Some((buffer, cursor)) = input_before_accept {
                self.state.update(cx, |overlay, cx| {
                    overlay.mark_suppress_unchanged_completion(buffer, cursor);
                    cx.notify();
                });
            }
        }
        self.hide(cx);
        self.sync_intercept(figterm_state, cx);
    }

    fn insert_text(
        &self,
        insertion: &str,
        deletion: i64,
        execute: bool,
        figterm_state: &FigtermState,
        _cx: &mut App,
    ) -> bool {
        let Some(session_id) = self.current_session() else {
            return false;
        };
        let Some(sender) = figterm_state.with(&session_id, |session| session.sender.clone()) else {
            warn!(%session_id, "no figterm session for insert");
            return false;
        };
        let snapshot = self.current_input_snapshot();
        *self.self_insertion.lock().unwrap_or_else(|err| err.into_inner()) = snapshot
            .as_ref()
            .and_then(|(buffer, cursor)| predicted_buffer_after_insert(buffer, *cursor, insertion, deletion))
            .map(|(predicted, _)| predicted);
        let insertion_buffer = snapshot.map(|(buffer, _)| buffer);
        if let Err(err) = sender.send(FigtermCommand::InsertText {
            insertion: Some(insertion.to_string()),
            deletion: Some(deletion),
            offset: None,
            // An auto-execute suggestion already carries its `\n` insertValue;
            // adding the immediate carriage return would execute twice.
            immediate: Some(immediate_for_insert(execute, insertion)),
            insertion_buffer,
            insert_during_command: None,
        }) {
            error!(%err, "failed to insert suggestion");
            false
        } else {
            fig_telemetry::count("autocomplete_accepted");
            true
        }
    }

    fn current_input_snapshot(&self) -> Option<(String, u32)> {
        let current_session = self.current_session()?;
        self.last_input
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
            .filter(|input| input.session_id == current_session)
            .map(|input| (input.buffer.clone(), input.cursor))
    }
}

fn apply_settings(overlay: &mut OverlayState) {
    let metrics = overlay_metrics();
    overlay.theme = resolve_overlay_theme();
    overlay.font_family = metrics.font_family;
    overlay.custom_font_family = metrics.custom_font_family;
    overlay.font_size = metrics.font_size;
    overlay.row_height = metrics.row_height;
    let configured_width = fig_settings::settings::get_int("autocomplete.width").ok().flatten();
    let configured_history_mode = fig_settings::settings::get_string("beta.history.mode").ok().flatten();
    overlay.history_setting = configured_history_mode.clone().unwrap_or_else(|| "show".into());
    overlay.list_width = legacy_list_width(configured_width, configured_history_mode.as_deref()).max(150) as f32;
    overlay.max_list_height =
        fig_settings::settings::get_int_or("autocomplete.height", DEFAULT_MAX_LIST_HEIGHT as i64).max(40) as f32;
    overlay.fuzzy_search = fig_settings::settings::get_bool_or("autocomplete.fuzzySearch", true);
    overlay.effective_fuzzy_search = overlay.fuzzy_search;
    overlay.only_show_on_tab = fig_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false);
    overlay.first_token_completion = fig_settings::settings::get_bool_or("autocomplete.firstTokenCompletion", false);
    overlay.scroll_wrap_around = fig_settings::settings::get_bool_or("autocomplete.scrollWrapAround", false);
    overlay.navigate_to_history = fig_settings::settings::get_bool_or("autocomplete.navigateToHistory", false);
    overlay.insert_space_automatically =
        fig_settings::settings::get_bool_or("autocomplete.insertSpaceAutomatically", true);
    let was_always_show = overlay.always_show_description;
    overlay.always_show_description = fig_settings::settings::get_bool_or("autocomplete.alwaysShowDescription", false);
    if overlay.always_show_description {
        overlay.description_popout = true;
    } else if was_always_show {
        overlay.description_popout = false;
    }
    overlay.show_dev_banner = fig_settings::settings::get_bool_or("autocomplete.developerModeNPM", false);
    overlay.description_hint = description_hint_from_settings();
}

fn legacy_list_width(configured_width: Option<i64>, configured_history_mode: Option<&str>) -> i64 {
    configured_width.unwrap_or_else(|| {
        let history_extra = if matches!(configured_history_mode, Some("show" | "history_only")) {
            50
        } else {
            0
        };
        DEFAULT_WIDTH as i64 + history_extra
    })
}

fn description_hint_from_settings() -> String {
    let bindings = KeyBindings::load_from_settings("autocomplete").unwrap_or_else(|_| KeyBindings(Vec::new()));
    bindings
        .into_iter()
        .find(|binding| {
            matches!(
                binding.identifier.as_str(),
                "toggleDescription" | "showDescription" | "hideDescription"
            )
        })
        .map(|binding| format_keybinding(&binding.binding))
        .filter(|binding| !binding.is_empty())
        .unwrap_or_else(|| "⌃k".into())
}

fn format_keybinding(binding: &str) -> String {
    binding
        .split('+')
        .map(|token| match token.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "meta" => "⌘".to_string(),
            "control" | "ctrl" => "⌃".to_string(),
            "shift" => "⇧".to_string(),
            "option" | "opt" | "alt" => "⌥".to_string(),
            "enter" | "return" => "↵".to_string(),
            "tab" => "⇥".to_string(),
            "escape" | "esc" => "⎋".to_string(),
            other => other.to_string(),
        })
        .collect()
}

struct OverlayMetrics {
    font_family: String,
    custom_font_family: bool,
    font_size: f32,
    row_height: f32,
}

fn overlay_metrics() -> OverlayMetrics {
    let configured_font = fig_settings::settings::get_string("autocomplete.fontFamily")
        .ok()
        .flatten()
        .filter(|font| !font.trim().is_empty());
    let custom_font_family = configured_font.is_some();
    let font_family = configured_font.unwrap_or_else(|| "Monaco".into());
    let font_size = {
        let setting = fig_settings::settings::get_int_or("autocomplete.fontSize", 0);
        if setting > 0 { setting as f32 } else { DEFAULT_FONT_SIZE }
    };
    OverlayMetrics {
        font_family,
        custom_font_family,
        font_size,
        row_height: font_size * 1.5625,
    }
}

fn overlay_window_size_from(overlay: &OverlayState) -> LogicalSize<f64> {
    let (width, height) = overlay_content_size_with_context(
        overlay.items.len(),
        overlay.effective_row_height(),
        overlay.effective_font_size(),
        overlay.effective_list_width(),
        overlay.effective_max_list_height(),
        overlay.description_popout && !overlay.items.is_empty() && !overlay.loading,
        overlay.show_dev_banner,
        overlay.loading,
        overlay.current_arg_rows(),
    );
    LogicalSize::new(f64::from(width), f64::from(height))
}

pub fn resolve_overlay_theme() -> OverlayTheme {
    let name = fig_settings::settings::get_string_or("autocomplete.theme", "github-dark".into());
    match name.to_ascii_lowercase().as_str() {
        "light" => OverlayTheme::light(),
        "dark" => OverlayTheme::dark(),
        "system" => {
            if ec_gpui::system_appearance_is_dark() {
                OverlayTheme::dark()
            } else {
                OverlayTheme::light()
            }
        },
        other => load_named_theme(other).unwrap_or_else(OverlayTheme::dark),
    }
}

fn load_named_theme(name: &str) -> Option<OverlayTheme> {
    static CACHE: OnceLock<Mutex<HashMap<String, OverlayTheme>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(theme) = guard.get(name) {
            return Some(*theme);
        }
    }
    let ctx = fig_os_shim::Context::new();
    let path = fig_util::directories::themes_dir(&ctx)
        .ok()?
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let theme = theme_from_json(&text)?;
    if let Ok(mut guard) = cache.lock() {
        guard.insert(name.to_string(), theme);
    }
    Some(theme)
}

#[allow(clippy::too_many_arguments)]
fn apply_complete_result(
    state: Entity<OverlayState>,
    window_slot: &Mutex<Option<OverlayHandle>>,
    result: anyhow::Result<CompleteResult>,
    session_id: Uuid,
    figterm_state: &FigtermState,
    cwd: &str,
    last_position: &Mutex<Option<WindowPosition>>,
    platform_state: &PlatformState,
    cx: &mut App,
) {
    match result {
        Ok(result) => {
            let mut uncached_icons = 0;
            let effective_fuzzy = result.fuzzy;
            let (current_arg_name, current_arg_description) = result
                .current_arg
                .as_ref()
                .map(|arg| (arg.name.clone(), arg.description.clone()))
                .unwrap_or_default();
            let items = result
                .suggestions
                .into_iter()
                .map(|s| SuggestionItem {
                    icon_png: file_icon_png(cwd, &s.name, &s.kind, &mut uncached_icons),
                    name: s.name,
                    description: s.description,
                    kind: s.kind,
                    args_hint: s.args_hint,
                    insert_value: s.insert_value,
                    display_name: s.display_name,
                    primary_name: s.primary_name,
                    separator_to_add: s.separator_to_add,
                    should_add_space: s.should_add_space,
                    hidden: s.hidden,
                    priority: s.priority,
                    icon_identifier: s.icon,
                    original_type: s.original_type,
                    query_term: s.query_term,
                })
                .collect::<Vec<_>>();
            let empty = items.is_empty();
            let match_term = if result.match_term.is_empty() {
                result.search_term.clone()
            } else {
                result.match_term
            };
            state.update(cx, |overlay, cx| {
                overlay.effective_fuzzy_search = effective_fuzzy;
                overlay.set_current_arg(current_arg_name, current_arg_description);
                overlay.set_suggestions_with_match_term(items, result.search_term, match_term);
                cx.notify();
            });
            let (visible, has_items) = {
                let overlay = state.read(cx);
                (overlay.visible, !overlay.items.is_empty())
            };
            let positioned = if visible {
                if let Some(handle) = ensure_overlay_window(window_slot, &state, cx) {
                    let screens = overlay_screens();
                    layout_overlay(&state, window_slot, handle, last_position, platform_state, &screens, cx)
                } else {
                    false
                }
            } else {
                if empty {
                    let _ = park_overlay_slot(window_slot, cx);
                }
                false
            };
            let has_last_position = last_position.lock().unwrap_or_else(|err| err.into_inner()).is_some();
            let (visible_intercept, global_intercept) = positioned_intercept_flags(InterceptInputs {
                overlay_visible: visible,
                has_items,
                positioned,
                has_last_position,
            });
            set_intercept_flags(figterm_state, session_id, visible_intercept, global_intercept);
            if visible && positioned && has_items {
                fig_telemetry::count("autocomplete_shown");
            }
        },
        Err(err) => {
            warn!(%err, "completion engine failed");
            state.update(cx, |overlay, cx| {
                overlay.dismiss();
                cx.notify();
            });
            let _ = park_overlay_slot(window_slot, cx);
            set_intercept(figterm_state, session_id, false, false);
        },
    }
}

fn ensure_overlay_window(
    slot: &Mutex<Option<OverlayHandle>>,
    state: &Entity<OverlayState>,
    cx: &mut App,
) -> Option<OverlayHandle> {
    if let Some(handle) = *slot.lock().unwrap_or_else(|err| err.into_inner()) {
        return Some(handle);
    }
    match open_overlay_window(cx, state.clone()) {
        Ok(handle) => {
            *slot.lock().unwrap_or_else(|err| err.into_inner()) = Some(handle);
            Some(handle)
        },
        Err(err) => {
            error!(%err, "Failed to open overlay window");
            None
        },
    }
}

fn clear_overlay_handle_if(slot: &Mutex<Option<OverlayHandle>>, handle: OverlayHandle) {
    let mut current = slot.lock().unwrap_or_else(|err| err.into_inner());
    if current.as_ref().is_some_and(|candidate| *candidate == handle) {
        *current = None;
    }
}

fn park_overlay_slot(slot: &Mutex<Option<OverlayHandle>>, cx: &mut App) -> bool {
    let Some(handle) = *slot.lock().unwrap_or_else(|err| err.into_inner()) else {
        return true;
    };
    match park_overlay_handle(&handle, cx) {
        Ok(()) => true,
        Err(err) => {
            warn!(%err, "overlay window update failed while parking; clearing stale handle");
            clear_overlay_handle_if(slot, handle);
            false
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_overlay(
    state: &Entity<OverlayState>,
    window_slot: &Mutex<Option<OverlayHandle>>,
    handle: OverlayHandle,
    last_position: &Mutex<Option<WindowPosition>>,
    platform_state: &PlatformState,
    screens: &[(f64, f64, f64, f64)],
    cx: &mut App,
) -> bool {
    let (overlay_size, popout, visible, empty, flip_height) = {
        let overlay = state.read(cx);
        (
            overlay_window_size_from(overlay),
            overlay.description_popout && !overlay.items.is_empty() && !overlay.loading,
            overlay.visible,
            overlay.items.is_empty() && !overlay.loading && !overlay.has_current_arg(),
            overlay.effective_max_list_height() as f64,
        )
    };
    if !visible || empty {
        return false;
    }
    if let Some(position) = *last_position.lock().unwrap_or_else(|err| err.into_inner()) {
        #[cfg(not(target_os = "macos"))]
        if matches!(position, WindowPosition::RelativeToCaret { .. }) && screens.is_empty() {
            let _ = park_overlay_slot(window_slot, cx);
            return false;
        }
        let (origin, size, on_left, is_above) =
            overlay_bounds(position, overlay_size, platform_state, popout, flip_height, screens);
        state.update(cx, |overlay, cx| {
            // The legacy view also retained `isAboveCursor` for the developer
            // banner when no description popout was open. Keeping the real
            // placement flag here avoids rendering that banner below a panel
            // that has already flipped above the caret.
            if overlay.set_layout_flags(on_left, is_above) {
                cx.notify();
            }
        });
        match position_overlay(origin, size, &handle, cx) {
            Ok(()) => true,
            Err(err) => {
                warn!(%err, "native overlay frame was not applied; retry next layout");
                let _ = park_overlay_handle(&handle, cx);
                ec_gpui::invalidate_cached_overlay_x_window();
                false
            },
        }
    } else {
        // A caret we never received is not a usable screen-space fallback: a
        // window-relative guess lands the list away from the real cursor. Keep
        // the last valid position when there is one, otherwise stay hidden.
        let _ = park_overlay_slot(window_slot, cx);
        false
    }
}

#[cfg(target_os = "macos")]
const MAX_UNCACHED_FILE_ICONS: usize = 8;

fn file_icon_png(cwd: &str, name: &str, kind: &str, uncached: &mut usize) -> Option<Arc<gpui::Image>> {
    if kind != "file" && kind != "folder" {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        use std::collections::HashMap;
        use std::sync::OnceLock;
        static CACHE: OnceLock<Mutex<HashMap<String, Arc<gpui::Image>>>> = OnceLock::new();
        let mut path = PathBuf::from(if cwd.is_empty() { "." } else { cwd });
        path.push(name.trim_end_matches('/'));
        let key = path.display().to_string();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(guard) = cache.lock() {
            if let Some(image) = guard.get(&key) {
                return Some(image.clone());
            }
        }
        if *uncached >= MAX_UNCACHED_FILE_ICONS {
            return None;
        }
        *uncached += 1;
        let bytes = unsafe { macos_utils::image::png_for_path(&path) }?;
        let image = Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, bytes));
        if let Ok(mut guard) = cache.lock() {
            if guard.len() >= 64 {
                if let Some(old) = guard.keys().next().cloned() {
                    guard.remove(&old);
                }
            }
            guard.insert(key, image.clone());
        }
        Some(image)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cwd, name, uncached);
        None
    }
}

fn session_changed(current: Option<Uuid>, next: Uuid) -> bool {
    current != Some(next)
}

fn should_skip_duplicate_input(previous: Option<&LastInput>, next: &LastInput, force: bool) -> bool {
    !force && previous == Some(next)
}

/// Sentinel for "no request owns the loading marker". Generations start at 1.
const NO_LOADING_OWNER: u64 = 0;

/// Whether a completion for `generation` must switch the loading marker off.
///
/// A request always releases its own latch. It also releases one left behind by
/// an older generation: generations only move forward, so an older owner has
/// already reported or been superseded and will never come back to clean up. An
/// owner newer than this result is left alone — that request is still running
/// and its marker is the one the user should be looking at.
fn loading_owner_is_released_by(owner: u64, generation: u64) -> bool {
    owner != NO_LOADING_OWNER && owner <= generation
}

fn completion_is_current(
    generation: u64,
    current_generation: u64,
    current_session: Option<Uuid>,
    result_session: Uuid,
) -> bool {
    generation == current_generation && current_session == Some(result_session)
}

fn action_session_is_current(current_session: Option<Uuid>, action_session: Uuid) -> bool {
    current_session == Some(action_session)
}

/// The legacy overlay did not offer completions while the caret was in the
/// middle of a token. It only completed the text to the left when the next
/// character was whitespace (or the caret was at the end of the buffer).
fn cursor_is_inside_word(buffer: &str, cursor: u32) -> bool {
    let Ok(mut cursor) = usize::try_from(cursor) else {
        return false;
    };
    cursor = cursor.min(buffer.len());
    while cursor < buffer.len() && !buffer.is_char_boundary(cursor) {
        cursor += 1;
    }
    buffer[cursor..].chars().next().is_some_and(|ch| !ch.is_whitespace())
}

fn insertion_changes_buffer(insertion: &str, deletion: i64, execute: bool) -> bool {
    deletion != 0 || !insertion.is_empty() || execute
}

fn immediate_for_insert(execute: bool, insertion: &str) -> bool {
    execute && !insertion.ends_with('\n')
}

fn accepted_suggestion_key(item: &ClickInsert, input: Option<&(String, u32)>) -> Option<(String, String)> {
    let (buffer, cursor) = input?;
    Some((
        ranking_root_command(buffer, Some(*cursor)),
        item.primary_name.clone().unwrap_or_else(|| item.name.clone()),
    ))
}

/// Predict the edit-buffer notification produced by a regular insertion.
///
/// `cursor` is a byte offset into the shell buffer, while `deletion` counts
/// shell characters (the same units used by the backspaces sent to figterm).
/// The only control sequence currently emitted in an insertion is the left
/// arrow used for `{cursor}`; it moves the cursor without becoming buffer text.
fn predicted_buffer_after_insert(buffer: &str, cursor: u32, insertion: &str, deletion: i64) -> Option<(String, u32)> {
    let cursor = usize::try_from(cursor).ok()?;
    if cursor > buffer.len() || !buffer.is_char_boundary(cursor) {
        return None;
    }
    let deletion = usize::try_from(deletion).ok()?;
    let deletion_start = previous_char_boundary(buffer, cursor, deletion);
    let mut result = String::with_capacity(buffer.len().saturating_sub(cursor - deletion_start) + insertion.len());
    result.push_str(&buffer[..deletion_start]);

    let mut predicted_cursor = result.len();
    let mut offset = 0;
    while offset < insertion.len() {
        let remaining = insertion.get(offset..)?;
        if remaining.starts_with("\x1b[D") {
            predicted_cursor = previous_char_boundary(&result, predicted_cursor, 1);
            offset += "\x1b[D".len();
            continue;
        }
        let character = remaining.chars().next()?;
        result.push(character);
        predicted_cursor = result.len();
        offset += character.len_utf8();
    }
    result.push_str(&buffer[cursor..]);
    Some((result, u32::try_from(predicted_cursor).ok()?))
}

fn previous_char_boundary(text: &str, from: usize, count: usize) -> usize {
    let mut boundary = from;
    for _ in 0..count {
        boundary = text[..boundary]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }
    boundary
}

fn opens_new_arg(add_space: bool, should_add_space: bool, separator: Option<&str>) -> bool {
    (add_space && should_add_space) || separator.is_some()
}

fn should_suppress_after_insert(execute: bool, kind: &str, opens_new_arg: bool, text: &str) -> bool {
    !execute && !matches!(kind, "auto-execute" | "special") && !opens_new_arg && !text.ends_with([' ', '\n', '/'])
}

fn insertion_for(name: &str, search_term: &str) -> (String, i64) {
    if let Some(suffix) = name.strip_prefix(search_term) {
        (suffix.to_string(), 0)
    } else {
        (name.to_string(), search_term.chars().count() as i64)
    }
}

fn insertion_for_kind(name: &str, search_term: &str, kind: &str) -> (String, i64) {
    if matches!(kind, "auto-execute" | "special") {
        (name.to_string(), 0)
    } else {
        insertion_for(name, search_term)
    }
}

fn insertion_search_term<'a>(kind: &str, raw_search: &'a str, query_term: Option<&'a str>) -> &'a str {
    // Shortcut matching strips the leading `?`, but the accepted edit must
    // replace that prefix too. The WebView added one deletion explicitly;
    // using the raw token here is the same operation and also handles Unicode
    // by letting `insertion_for` count shell characters.
    if kind == "shortcut" {
        raw_search
    } else {
        query_term.unwrap_or(raw_search)
    }
}

#[cfg(test)]
fn full_insertion(name: &str, kind: &str, args_hint: &str, add_space: bool) -> String {
    let is_folder = matches!(kind, "folder" | "dir");
    let mut text = escape_insertion(name, is_folder);
    if add_space && should_add_space(kind, args_hint) && !text.ends_with([' ', '\n', '/']) {
        text.push(' ');
    }
    text
}

/// Build the accepted text using the metadata carried by a Fig suggestion.
/// `insert_value` wins over the display name for all non-file suggestions;
/// file/folder suggestions retain the shell escaping path from the old UI.
/// Did a backspace remove a whole trailing token? Fig compared the parsed
/// token arrays and kept the list down in that case, so deleting `-m` from
/// `git commit -m` does not immediately reopen every `git commit` option.
fn backspaced_to_new_token(previous: &str, current: &str) -> bool {
    if current.len() >= previous.len() {
        return false;
    }
    let current_tokens: Vec<&str> = current.split_whitespace().collect();
    let previous_tokens: Vec<&str> = previous.split_whitespace().collect();
    if current_tokens.is_empty() || current_tokens.len() >= previous_tokens.len() {
        return false;
    }
    previous_tokens[current_tokens.len() - 1] == current_tokens[current_tokens.len() - 1]
}

/// The WebView's `isBufferDifferenceFromTyping`, inverted. Anything that is
/// not a single-character edit on a shared prefix reads as a paste or a shell
/// history recall, and Fig did not answer those with suggestions.
fn large_buffer_change(previous: &str, current: &str) -> bool {
    if !previous.starts_with(current) && !current.starts_with(previous) {
        return true;
    }
    let previous_len = previous.chars().count() as i64;
    let current_len = current.chars().count() as i64;
    (previous_len - current_len).abs() >= 2
}

/// Does the escaped common prefix already spell out the row's own insertion?
/// Fig compared the two before deciding whether Tab was a partial completion
/// or a full acceptance, using the bare insertion — without the trailing space
/// or newline an acceptance would append.
fn prefix_completes_row(shared: &str, item: &SuggestionItem) -> bool {
    if item.kind == "auto-execute" {
        return false;
    }
    shared.replace(' ', r"\ ")
        == full_insertion_for_item(
            &item.name,
            item.insert_value.as_deref(),
            &item.kind,
            item.separator_to_add.as_deref(),
            false,
            false,
            false,
        )
}

fn full_insertion_for_item(
    name: &str,
    insert_value: Option<&str>,
    kind: &str,
    separator: Option<&str>,
    should_add_space: bool,
    add_space: bool,
    execute: bool,
) -> String {
    let is_folder = matches!(kind, "folder" | "dir");
    let is_file_or_folder = is_folder || kind == "file";
    let mut text = if let Some(value) = insert_value.filter(|_| !is_file_or_folder) {
        value.to_string()
    } else {
        let mut value = name.to_string();
        if let Some(separator) = separator.filter(|separator| !separator.is_empty()) {
            value.push_str(separator);
            value.push_str("{cursor}");
        }
        // The WebView escaped the completed value, including the separator
        // and cursor marker. This keeps the closing quote after `{cursor}` so
        // resolving the marker leaves the caret inside a quoted value.
        escape_insertion(&value, is_folder)
    };

    if execute && kind != "auto-execute" {
        text.push('\n');
    }
    if text.ends_with('\n') {
        let end = text.trim_end_matches('\n').len();
        text.truncate(end);
        text.push('\n');
    }
    if add_space && should_add_space && !text.ends_with('\n') {
        text.push(' ');
    }
    text
}

/// Fig's `{cursor}` marker is inserted as text followed by left-arrow bytes.
/// The PTY insertion request applies its `offset` before insertion, so putting
/// the movement in the insertion stream preserves the old post-insert cursor
/// position (notably for `--option={cursor}` and quoted snippets).
fn resolve_cursor_marker(mut text: String) -> String {
    const MARKER: &str = "{cursor}";
    let Some(marker) = text.find(MARKER) else {
        return text;
    };
    let after = text[marker + MARKER.len()..].chars().count();
    text.replace_range(marker..marker + MARKER.len(), "");
    text.push_str(&"\x1b[D".repeat(after));
    text
}

#[cfg(test)]
fn should_add_space(kind: &str, args_hint: &str) -> bool {
    match kind {
        "cmd" => true,
        "subcommand" | "option" => args_hint.contains('<'),
        _ => false,
    }
}

fn escape_insertion(value: &str, is_folder: bool) -> String {
    const SPECIAL: &[char] = &[
        '\\', '?', '*', '\'', '"', '#', '|', '<', '>', '(', ')', '[', ']', '!', '&',
    ];
    if !value.chars().any(|ch| SPECIAL.contains(&ch)) {
        // JavaScript's `str.replace(/\s/g, "\\ ")` normalizes every
        // whitespace character to an escaped ASCII space.
        let mut escaped = String::with_capacity(value.len());
        for ch in value.chars() {
            if ch.is_whitespace() {
                escaped.push_str(r"\ ");
            } else {
                escaped.push(ch);
            }
        }
        return escaped;
    }
    let quoted = |s: &str| s.replace('\'', r#"'"'"'"#);
    if is_folder {
        let body = value.strip_suffix('/').unwrap_or(value);
        format!("'{}'/", quoted(body))
    } else {
        format!("'{}'", quoted(value))
    }
}

fn intercept_flags(overlay_visible: bool, has_items: bool) -> (bool, bool) {
    // Keystrokes only while the list is on screen. Global intercept stays on when
    // items are kept (onlyShowOnTab / hideAutocomplete) so Tab can show the list.
    // Without a caret sample this assumes a usable last position whenever
    // rows exist — `sync_intercept` passes the real last-position bit.
    positioned_intercept_flags(InterceptInputs {
        overlay_visible,
        has_items,
        positioned: overlay_visible,
        has_last_position: has_items,
    })
}

/// Grouped so the four independent flags cannot be silently transposed at a
/// call site.
#[derive(Clone, Copy)]
struct InterceptInputs {
    overlay_visible: bool,
    has_items: bool,
    positioned: bool,
    has_last_position: bool,
}

/// Native key interception must additionally know whether a caret location is
/// usable. A hidden list with retained rows may keep global Tab interception
/// when it can be shown at the last caret; a visible list that failed layout
/// must not swallow terminal keys at all.
fn positioned_intercept_flags(inputs: InterceptInputs) -> (bool, bool) {
    let InterceptInputs {
        overlay_visible,
        has_items,
        positioned,
        has_last_position,
    } = inputs;
    if !has_items {
        return (false, false);
    }
    if overlay_visible {
        (positioned, positioned)
    } else {
        (false, has_last_position)
    }
}

fn set_intercept(figterm_state: &FigtermState, session_id: Uuid, overlay_visible: bool, has_items: bool) {
    let (intercept, intercept_global) = intercept_flags(overlay_visible, has_items);
    set_intercept_flags(figterm_state, session_id, intercept, intercept_global);
}

/// `None` when figterm already has this intercept pair, so the overlay can
/// skip two IPC frames (and a settings reload for the action list) on every
/// keystroke that did not change visibility.
fn next_intercept_modes(
    current_intercept: InterceptMode,
    current_global: InterceptMode,
    enable: bool,
    enable_global: bool,
) -> Option<(InterceptMode, InterceptMode)> {
    let intercept = InterceptMode::from(enable);
    let intercept_global = InterceptMode::from(enable_global);
    (current_intercept != intercept || current_global != intercept_global).then_some((intercept, intercept_global))
}

fn set_intercept_flags(figterm_state: &FigtermState, session_id: Uuid, intercept: bool, intercept_global: bool) {
    for session in figterm_state.inner.lock().linked_sessions.values_mut() {
        let for_this = session.id == session_id;
        let enable = intercept && for_this;
        let enable_global = intercept_global && for_this;
        let Some((next_intercept, next_global)) =
            next_intercept_modes(session.intercept, session.intercept_global, enable, enable_global)
        else {
            continue;
        };
        session.intercept = next_intercept;
        session.intercept_global = next_global;
        let actions = if enable || enable_global {
            overlay_actions()
        } else {
            vec![]
        };
        let _ = session.sender.send(FigtermCommand::InterceptFigJs {
            intercept_keystrokes: enable,
            intercept_global_keystrokes: enable_global,
            actions,
            override_actions: enable || enable_global,
        });
        let _ = session
            .sender
            .send(FigtermCommand::InterceptFigJSVisible { visible: enable });
    }
}

fn action_requires_visible(action: &str) -> bool {
    matches!(
        action,
        "navigateUp"
            | "navigateDown"
            | "insertSelected"
            | "insertCommonPrefixOrInsertSelected"
            | "insertSelectedAndExecute"
            | "execute"
            | "insertCommonPrefix"
            | "insertCommonPrefixOrNavigateDown"
            | "toggleDescription"
            | "showDescription"
            | "hideDescription"
            | "toggleHistoryMode"
            | "toggleFuzzySearch"
            | "increaseSize"
            | "decreaseSize"
    ) || action.starts_with("selectSuggestion")
}

fn action_is_allowed(action: &str, visible: bool, loading: bool) -> bool {
    if loading && action == "showAutocompleteFromTab" {
        return false;
    }
    !action_requires_visible(action) || (visible && !loading)
}

fn click_matches_current(click: &ClickInsert, overlay: &OverlayState) -> bool {
    overlay.visible
        && !overlay.loading
        && click.search == overlay.search_term
        && overlay.items.iter().any(|item| {
            item.name == click.name
                && item.description == click.description
                && item.kind == click.kind
                && item.insert_value == click.insert_value
                && item.primary_name == click.primary_name
                && item.separator_to_add == click.separator_to_add
                && item.should_add_space == click.should_add_space
                && item.query_term == click.query_term
        })
}

fn select_suggestion_index(n: usize, len: usize) -> Option<usize> {
    (n >= 1 && n <= len).then(|| n - 1)
}

const DEFAULT_OVERLAY_BINDINGS: &[(&str, &[&str])] = &[
    ("insertSelected", &["enter"]),
    ("insertCommonPrefix", &["tab"]),
    ("hideAutocomplete", &["esc"]),
    ("navigateUp", &["shift+tab", "up", "control+p"]),
    ("navigateDown", &["down", "control+n"]),
    ("selectSuggestion1", &["command+1"]),
    ("selectSuggestion2", &["command+2"]),
    ("selectSuggestion3", &["command+3"]),
    ("selectSuggestion4", &["command+4"]),
    ("selectSuggestion5", &["command+5"]),
    ("selectSuggestion6", &["command+6"]),
    ("selectSuggestion7", &["command+7"]),
    ("selectSuggestion8", &["command+8"]),
    ("selectSuggestion9", &["command+9"]),
    ("selectSuggestion10", &["command+0"]),
    ("toggleDescription", &["control+k"]),
    ("toggleHistoryMode", &["control+r"]),
];

fn overlay_actions() -> Vec<Action> {
    let user = KeyBindings::load_from_settings("autocomplete").unwrap_or_else(|_| KeyBindings(Vec::new()));
    merge_overlay_actions(DEFAULT_OVERLAY_BINDINGS, user)
}

fn merge_overlay_actions(defaults: &[(&str, &[&str])], user: KeyBindings) -> Vec<Action> {
    let mut actions: Vec<Action> = defaults
        .iter()
        .map(|(identifier, bindings)| Action {
            identifier: (*identifier).to_string(),
            bindings: bindings.iter().map(|binding| (*binding).to_string()).collect(),
        })
        .collect();
    for KeyBinding { identifier, binding } in user {
        actions.push(Action {
            identifier,
            bindings: vec![binding],
        });
    }
    actions
}

fn overlay_bounds(
    position: WindowPosition,
    overlay_size: LogicalSize<f64>,
    platform_state: &PlatformState,
    popout: bool,
    flip_height: f64,
    screens: &[(f64, f64, f64, f64)],
) -> (Point<Pixels>, Size<Pixels>, bool, bool) {
    match position {
        WindowPosition::Absolute(pos) => {
            let logical: LogicalPosition<f64> = match pos {
                Position::Logical(p) => LogicalPosition::new(p.x, p.y),
                Position::Physical(p) => p.to_logical(1.0),
            };
            (
                gpui::point(px(logical.x as f32), px(logical.y as f32)),
                size(px(overlay_size.width as f32), px(overlay_size.height as f32)),
                false,
                false,
            )
        },
        WindowPosition::Centered => (
            gpui::point(px(120.), px(120.)),
            size(px(overlay_size.width as f32), px(overlay_size.height as f32)),
            false,
            false,
        ),
        WindowPosition::RelativeToCaret {
            caret_position,
            caret_size,
            origin,
        } => {
            let mut caret: LogicalPosition<f64> = match caret_position {
                Position::Logical(p) => LogicalPosition::new(p.x, p.y),
                Position::Physical(p) => p.to_logical(1.0),
            };
            let caret_size: LogicalSize<f64> = match caret_size {
                tao::dpi::Size::Logical(s) => LogicalSize::new(s.width, s.height),
                tao::dpi::Size::Physical(s) => s.to_logical(1.0),
            };
            caret.y = caret_y_in_screen_space(caret.y, caret_size.height, origin, screens.first().copied());
            let mut edges = screen_edges_containing(screens, caret.x, caret.y);
            let mut flip_bottom = edges.map(|(_, _, _, bottom)| bottom);
            let window_bottom = platform_state
                .get_active_window()
                .map(|active| window_edges(&active.rect).3);
            (edges, flip_bottom) = tighten_flip_bottom_with_terminal(edges, flip_bottom, window_bottom);
            let (place_width, place_height, place_flip) =
                overlay_size_in_screen_space(overlay_size.width, overlay_size.height, flip_height);
            let (x, y, on_left, is_above) = place_overlay_at_caret(
                caret.x,
                caret.y,
                caret_size.height,
                place_width,
                place_height,
                place_flip,
                edges,
                flip_bottom,
                popout,
            );
            (
                gpui::point(px(x as f32), px(y as f32)),
                size(px(overlay_size.width as f32), px(overlay_size.height as f32)),
                on_left,
                is_above,
            )
        },
    }
}

fn overlay_size_in_screen_space(width: f64, height: f64, flip_height: f64) -> (f64, f64, f64) {
    let scale = overlay_placement_scale_or_one(ec_gpui::overlay_placement_scale());
    (width * scale, height * scale, flip_height * scale)
}

fn overlay_placement_scale_or_one(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 { scale } else { 1.0 }
}

/// Place the overlay below the caret, flipping the description to the left or the
/// window above the caret when it would leave `edges` (left, top, right, bottom).
///
/// `flip_bottom` is the lower of the screen and the terminal window; it only
/// decides whether to flip. Clamping always uses the screen `edges`.
#[allow(clippy::too_many_arguments)]
fn place_overlay_at_caret(
    caret_x: f64,
    caret_y: f64,
    caret_height: f64,
    overlay_width: f64,
    overlay_height: f64,
    flip_height: f64,
    edges: Option<(f64, f64, f64, f64)>,
    flip_bottom: Option<f64>,
    popout: bool,
) -> (f64, f64, bool, bool) {
    let mut x = caret_x;
    let mut y = caret_y + caret_height + 2.0;
    let mut on_left = false;
    let mut is_above = false;
    let constrained_to_screen = edges.is_some();
    if let Some((left, top, right, bottom)) = edges {
        if popout && x + overlay_width > right {
            let shift = (ec_gpui::POPOUT_WIDTH as f64) * ec_gpui::overlay_placement_scale();
            x = (caret_x - shift).max(left);
            on_left = true;
        }
        // The WebView decided whether to flip using the configured maxHeight,
        // not the momentary number of rendered rows. This keeps a short list
        // from jumping below the caret only to move above it as more generator
        // results arrive.
        let overflows_above = top >= caret_y - flip_height;
        let overflows_below = caret_y + caret_height + flip_height > flip_bottom.unwrap_or(bottom);
        if !overflows_above && overflows_below {
            y = caret_y - overlay_height - 2.0;
            is_above = true;
        }
        // Keep the whole window on the monitor, exactly as the WebView host did.
        x = x.min((right - overlay_width).max(left)).max(left);
        y = y.min((bottom - overlay_height).max(top)).max(top);
    }
    if !constrained_to_screen {
        x = x.max(0.0);
        y = y.max(0.0);
    }
    (x, y, on_left, is_above)
}

/// Convert caret Y into top-left screen space (the overlay's placement space).
/// `Origin::BottomLeft` is Cocoa / macOS IME; `Origin::TopLeft` is already
/// screen Y on every backend.
fn caret_y_in_screen_space(
    caret_y: f64,
    caret_height: f64,
    origin: Origin,
    primary_screen: Option<(f64, f64, f64, f64)>,
) -> f64 {
    match (origin, primary_screen) {
        (Origin::BottomLeft, Some((_, primary_y, _, primary_height))) => {
            primary_y + primary_height - caret_y - caret_height
        },
        _ => caret_y,
    }
}

fn window_edges(rect: &crate::utils::Rect) -> (f64, f64, f64, f64) {
    let pos = match rect.position {
        Position::Logical(p) => (p.x, p.y),
        Position::Physical(p) => {
            let p = p.to_logical(1.0);
            (p.x, p.y)
        },
    };
    let size = match rect.size {
        tao::dpi::Size::Logical(s) => (s.width, s.height),
        tao::dpi::Size::Physical(s) => {
            let s = s.to_logical(1.0);
            (s.width, s.height)
        },
    };
    (pos.0, pos.1, pos.0 + size.0, pos.1 + size.1)
}

#[allow(clippy::type_complexity)]
fn tighten_flip_bottom_with_terminal(
    edges: Option<(f64, f64, f64, f64)>,
    flip_bottom: Option<f64>,
    window_bottom: Option<f64>,
) -> (Option<(f64, f64, f64, f64)>, Option<f64>) {
    match (edges, flip_bottom, window_bottom) {
        (Some(edges), Some(bottom), Some(window_bottom)) => (Some(edges), Some(bottom.min(window_bottom))),
        (edges, flip_bottom, _) => (edges, flip_bottom),
    }
}

fn screen_edges_containing(screens: &[(f64, f64, f64, f64)], x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
    let screen = screens
        .iter()
        .copied()
        .find(|(sx, sy, sw, sh)| x >= *sx && x <= sx + sw && y >= *sy && y <= sy + sh)
        .or_else(|| screens.first().copied())?;
    Some((screen.0, screen.1, screen.0 + screen.2, screen.1 + screen.3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_keeps_directory_prefix() {
        assert_eq!(insertion_for("src/main.rs", "src/m"), ("ain.rs".into(), 0));
    }

    #[test]
    fn insert_appends_history_suffix() {
        assert_eq!(
            insertion_for("git checkout -b feature", "git"),
            (" checkout -b feature".into(), 0)
        );
    }

    #[test]
    fn insert_replaces_token_when_name_is_not_a_prefix() {
        assert_eq!(insertion_for("checkout", "git"), ("checkout".into(), 3));
    }

    #[test]
    fn shortcut_acceptance_replaces_the_raw_question_mark_query() {
        let search = insertion_search_term("shortcut", "?la", Some("la"));
        assert_eq!(search, "?la");
        assert_eq!(insertion_for("last", search), ("last".into(), 3));
        assert_eq!(insertion_search_term("arg", "scope@la", Some("la")), "la");
    }

    #[test]
    fn insert_deletion_counts_unicode_chars_not_bytes() {
        assert_eq!(insertion_for("文件.txt", "文"), ("件.txt".into(), 0));
        assert_eq!(insertion_for("src/", "文"), ("src/".into(), 1));
    }

    #[test]
    fn insert_replaces_token_when_case_differs() {
        assert_eq!(insertion_for("Checkout", "ch"), ("Checkout".into(), 2));
    }

    #[test]
    fn completion_is_hidden_when_cursor_is_inside_a_word() {
        assert!(cursor_is_inside_word("git status", 5));
        assert!(!cursor_is_inside_word("git status", 3));
        assert!(!cursor_is_inside_word("git status", 10));
        assert!(cursor_is_inside_word("git 文件", 5));
    }

    #[test]
    fn legacy_width_only_adds_history_space_for_an_unset_width() {
        assert_eq!(legacy_list_width(None, None), 320);
        assert_eq!(legacy_list_width(None, Some("show")), 370);
        assert_eq!(legacy_list_width(None, Some("history_only")), 370);
        assert_eq!(legacy_list_width(None, Some("off")), 320);
        assert_eq!(legacy_list_width(Some(300), Some("show")), 300);
    }

    #[test]
    fn auto_execute_does_not_delete_the_exact_query() {
        assert_eq!(insertion_for_kind("\n", "status", "auto-execute"), ("\n".into(), 0));
    }

    #[test]
    fn ordinary_enter_is_not_an_immediate_execute() {
        assert!(!immediate_for_insert(false, "checkout"));
        assert!(immediate_for_insert(true, "checkout"));
        assert!(!immediate_for_insert(true, "\n"));
    }

    #[test]
    fn acceptance_uses_the_engine_root_and_primary_alias() {
        let item = ClickInsert {
            name: "co".into(),
            primary_name: Some("checkout".into()),
            ..ClickInsert::default()
        };
        assert_eq!(
            accepted_suggestion_key(&item, Some(&("git co ignored".into(), 6))),
            Some(("git".into(), "checkout".into()))
        );

        let item = ClickInsert {
            name: "status".into(),
            ..ClickInsert::default()
        };
        assert_eq!(
            accepted_suggestion_key(&item, Some(&("git status".into(), 10))),
            Some(("git".into(), "status".into()))
        );
        assert_eq!(accepted_suggestion_key(&item, None), None);
    }

    #[test]
    fn predicted_acceptance_tracks_buffer_and_cursor() {
        assert_eq!(
            predicted_buffer_after_insert("git co", 6, "mmit", 0),
            Some(("git commit".into(), 10))
        );
        assert_eq!(
            predicted_buffer_after_insert("git st", 6, "status", 2),
            Some(("git status".into(), 10))
        );
        assert_eq!(
            predicted_buffer_after_insert("--message", 9, "= \x1b[D", 0),
            Some(("--message= ".into(), 10))
        );
    }

    #[test]
    fn predicted_suppression_does_not_swallow_a_later_input_without_ack() {
        let Some((expected_buffer, expected_cursor)) = predicted_buffer_after_insert("git co", 6, "mmit", 0) else {
            panic!("expected a predictable insertion");
        };
        let mut overlay = OverlayState::new();
        overlay.mark_suppress_unchanged_completion(expected_buffer, expected_cursor);
        assert!(!overlay.take_suppress_completion_for("git commitx", 11));
        assert!(!overlay.take_suppress_completion_for("git commit", 10));
    }

    #[test]
    fn session_switch_is_detected_for_generation_invalidation() {
        let first = Uuid::nil();
        let second = Uuid::from_u128(1);
        assert!(session_changed(None, first));
        assert!(!session_changed(Some(first), first));
        assert!(session_changed(Some(first), second));
    }

    #[test]
    fn completion_requires_current_generation_and_session() {
        let session = Uuid::from_u128(7);
        assert!(completion_is_current(4, 4, Some(session), session));
        assert!(!completion_is_current(3, 4, Some(session), session));
        assert!(!completion_is_current(4, 4, None, session));
        assert!(!completion_is_current(4, 4, Some(Uuid::from_u128(8)), session));
    }

    #[test]
    fn actions_only_apply_to_the_current_session() {
        let current = Uuid::from_u128(7);
        assert!(action_session_is_current(Some(current), current));
        assert!(!action_session_is_current(None, current));
        assert!(!action_session_is_current(Some(Uuid::from_u128(8)), current));
    }

    #[test]
    fn empty_exact_acceptance_does_not_claim_the_buffer_changed_or_execute() {
        assert!(!insertion_changes_buffer("", 0, false));
        assert!(insertion_changes_buffer("checkout", 0, false));
        assert!(insertion_changes_buffer("", 3, false));
        assert!(insertion_changes_buffer("", 0, true));
    }

    #[test]
    fn new_argument_depends_on_effective_space_setting_or_separator() {
        assert!(opens_new_arg(true, true, None));
        assert!(!opens_new_arg(false, true, None));
        assert!(opens_new_arg(false, false, Some("=")));
        assert!(!opens_new_arg(true, false, None));
    }

    #[test]
    fn suppress_predicate_excludes_actions_and_new_arguments() {
        assert!(should_suppress_after_insert(false, "subcommand", false, "checkout"));
        assert!(!should_suppress_after_insert(true, "subcommand", false, "checkout"));
        assert!(!should_suppress_after_insert(false, "auto-execute", false, "\n"));
        assert!(!should_suppress_after_insert(false, "subcommand", true, "checkout "));
        assert!(!should_suppress_after_insert(false, "subcommand", false, "checkout "));
    }

    #[test]
    fn common_prefix_of_nested_paths() {
        assert_eq!(ec_gpui::longest_common_prefix(&["src/main.rs", "src/mod.rs"]), "src/m");
    }

    #[test]
    fn escape_spaces_in_file_names() {
        assert_eq!(escape_insertion("my file.txt", false), r"my\ file.txt");
        assert_eq!(escape_insertion("My Folder/", true), r"My\ Folder/");
    }

    #[test]
    fn escape_quotes_special_characters() {
        assert_eq!(escape_insertion("it's.txt", false), "'it'\"'\"'s.txt'");
        assert_eq!(escape_insertion("it's/", true), "'it'\"'\"'s'/");
    }

    #[test]
    fn space_after_first_token_commands_and_mandatory_args() {
        assert!(should_add_space("cmd", ""));
        assert!(should_add_space("subcommand", "<branch>"));
        assert!(!should_add_space("subcommand", "[branch]"));
        assert!(!should_add_space("file", "<unused>"));
        assert!(!should_add_space("history", ""));
    }

    #[test]
    fn full_insertion_appends_space_to_commands() {
        assert_eq!(full_insertion("git", "cmd", "", true), "git ");
        assert_eq!(full_insertion("checkout", "subcommand", "[branch]", true), "checkout");
        assert_eq!(full_insertion("src/main.rs", "file", "", true), "src/main.rs");
        assert_eq!(full_insertion("git", "cmd", "", false), "git");
    }

    #[test]
    fn metadata_insert_value_is_used_verbatim() {
        assert_eq!(
            full_insertion_for_item("co", Some("commit -m '{cursor}'"), "arg", None, false, true, false,),
            "commit -m '{cursor}'"
        );
    }

    #[test]
    fn metadata_separator_places_cursor_marker() {
        assert_eq!(
            full_insertion_for_item("--message", None, "option", Some("="), true, true, false),
            "--message={cursor} "
        );
    }

    #[test]
    fn separator_is_escaped_with_the_name_and_keeps_cursor_inside_quotes() {
        let text = full_insertion_for_item("it's", None, "option", Some("="), false, true, false);
        assert_eq!(resolve_cursor_marker(text), "'it'\"'\"'s='\x1b[D");
    }

    #[test]
    fn execute_appends_one_newline_and_never_an_automatic_space() {
        assert_eq!(
            full_insertion_for_item("checkout", None, "subcommand", None, true, true, true),
            "checkout\n"
        );
        assert_eq!(
            full_insertion_for_item("ignored", Some("status\n\n"), "arg", None, true, true, true),
            "status\n"
        );
        assert_eq!(
            full_insertion_for_item("execute", Some("\n"), "auto-execute", None, true, true, true),
            "\n"
        );
    }

    #[test]
    fn should_add_space_matches_legacy_without_extra_suffix_guards() {
        assert_eq!(
            full_insertion_for_item("ignored", Some("value "), "arg", None, true, true, false),
            "value  "
        );
        assert_eq!(
            full_insertion_for_item("folder/", None, "folder", None, true, true, false),
            "folder/ "
        );
    }

    #[test]
    fn cursor_marker_moves_after_trailing_quote() {
        assert_eq!(
            resolve_cursor_marker("commit -m '{cursor}'".into()),
            "commit -m ''\x1b[D"
        );
    }

    #[test]
    fn history_insert_value_keeps_literal_spaces() {
        assert_eq!(
            full_insertion_for_item(
                "git checkout -b feature",
                Some("git checkout -b feature"),
                "history",
                None,
                false,
                true,
                false,
            ),
            "git checkout -b feature"
        );
    }

    #[test]
    fn plain_insertion_normalizes_all_whitespace_like_javascript() {
        assert_eq!(escape_insertion("one\ttwo\nthree", false), r"one\ two\ three");
    }

    #[test]
    fn hide_turns_off_keystroke_intercept_but_keeps_tab_when_items_remain() {
        assert_eq!(intercept_flags(true, true), (true, true));
        assert_eq!(intercept_flags(false, true), (false, true));
        assert_eq!(intercept_flags(false, false), (false, false));
        assert_eq!(intercept_flags(true, false), (false, false));
    }

    #[test]
    fn unchanged_intercept_modes_skip_figterm_ipc() {
        use super::InterceptMode::{Locked, Unlocked};
        assert_eq!(next_intercept_modes(Unlocked, Unlocked, false, false), None);
        assert_eq!(next_intercept_modes(Locked, Locked, true, true), None);
        assert_eq!(
            next_intercept_modes(Unlocked, Unlocked, true, true),
            Some((Locked, Locked))
        );
        assert_eq!(
            next_intercept_modes(Locked, Locked, false, true),
            Some((Unlocked, Locked))
        );
        assert_eq!(
            next_intercept_modes(Unlocked, Locked, false, true),
            None,
            "hide-with-rows keeps global Tab intercept without a second SetFigjsIntercepts"
        );
        let src = include_str!("overlay.rs");
        let body = rust_fn_body(src, "fn set_intercept_flags");
        assert!(
            body.contains("next_intercept_modes") && body.contains("continue"),
            "set_intercept_flags must skip InterceptFigJs when the session already has that pair"
        );
        let skip = body.find("next_intercept_modes").expect("skip");
        let actions = body.find("overlay_actions()").expect("actions");
        assert!(
            skip < actions,
            "keybinding reload must sit behind the unchanged-mode skip"
        );
    }

    #[test]
    fn clear_autocomplete_cache_reaches_the_engine_client() {
        let production = include_str!("overlay.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            production.contains("pub fn clear_engine_caches") && production.contains("engine.clear_caches()"),
            "overlay must own the EngineClient cache-clear path"
        );
        assert!(
            !production.contains("blocking_recv") || production.contains("clear_caches().await"),
            "cache clear must not block the GPUI thread"
        );
    }

    #[test]
    fn intercept_flags_share_one_policy() {
        assert_eq!(
            intercept_flags(true, true),
            positioned_intercept_flags(InterceptInputs {
                overlay_visible: true,
                has_items: true,
                positioned: true,
                has_last_position: true,
            })
        );
        assert_eq!(
            intercept_flags(false, true),
            positioned_intercept_flags(InterceptInputs {
                overlay_visible: false,
                has_items: true,
                positioned: false,
                has_last_position: true,
            })
        );
        let src = include_str!("overlay.rs");
        let sync = rust_fn_body(src, "fn sync_intercept");
        assert!(
            sync.contains("positioned_intercept_flags"),
            "hide/dismiss intercept must use the same caret-aware policy as layout"
        );
        assert!(
            !sync.contains("overlay.visible && has_items && has_position"),
            "sync_intercept must not re-derive flags beside positioned_intercept_flags"
        );
    }

    #[test]
    fn a_terminal_window_does_not_stand_in_for_missing_screens() {
        let edges = None;
        let flip = None;
        assert_eq!(
            tighten_flip_bottom_with_terminal(edges, flip, Some(400.0)),
            (None, None)
        );
        assert_eq!(
            tighten_flip_bottom_with_terminal(Some((0.0, 0.0, 100.0, 800.0)), Some(800.0), Some(400.0)),
            (Some((0.0, 0.0, 100.0, 800.0)), Some(400.0))
        );
    }

    #[test]
    fn bottom_left_caret_needs_screens_top_left_does_not() {
        use crate::platform::caret::caret_origin_needs_screens;
        assert!(caret_origin_needs_screens(Origin::BottomLeft));
        assert!(!caret_origin_needs_screens(Origin::TopLeft));
    }

    fn rust_fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src.find(signature).unwrap_or_else(|| panic!("missing {signature}"));
        let rest = &src[start..];
        let brace = rest.find('{').expect("fn body");
        let bytes = rest.as_bytes();
        let mut depth = 0i32;
        for (i, &b) in bytes.iter().enumerate().skip(brace) {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &rest[..=i];
                    }
                },
                _ => {},
            }
        }
        rest
    }

    #[test]
    fn apply_position_and_layout_overlay_share_one_screen_list() {
        let src = include_str!("overlay.rs");
        let apply = rust_fn_body(src, "pub fn apply_position");
        assert!(
            apply.contains("let screens = overlay_screens();"),
            "apply_position must fetch the screen list once"
        );
        assert!(
            apply.contains("layout_overlay(") && apply.contains("&screens"),
            "apply_position must pass that list into layout_overlay"
        );
        let layout = rust_fn_body(src, "fn layout_overlay");
        assert!(
            !layout.contains("overlay_screens()"),
            "layout_overlay must not open a second screen list"
        );
        let bounds = rust_fn_body(src, "fn overlay_bounds");
        assert!(
            !bounds.contains("overlay_screens()"),
            "overlay_bounds must use the shared screen list, not fetch its own"
        );
        assert!(
            !layout.contains("window-rect") && !layout.contains("PositionRelativeToRect"),
            "caret placement must not fall back to the terminal window rect"
        );
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    #[test]
    fn stub_screens_are_empty_so_a_caret_cannot_be_placed() {
        assert!(
            overlay_screens().is_empty(),
            "platform_stub must not invent a screen list; overlay placement parks instead"
        );
    }

    #[test]
    fn native_intercept_never_swallows_keys_without_a_usable_caret() {
        let inputs = |overlay_visible, positioned, has_last_position| InterceptInputs {
            overlay_visible,
            has_items: true,
            positioned,
            has_last_position,
        };
        assert_eq!(positioned_intercept_flags(inputs(true, false, false)), (false, false));
        assert_eq!(positioned_intercept_flags(inputs(false, false, false)), (false, false));
        assert_eq!(positioned_intercept_flags(inputs(false, false, true)), (false, true));
        assert_eq!(positioned_intercept_flags(inputs(true, true, true)), (true, true));
    }

    #[test]
    fn explicit_recompute_bypasses_identical_buffer_deduplication() {
        let input = LastInput {
            buffer: "git add .".into(),
            cwd: "/tmp/project".into(),
            cursor: 9,
            session_id: Uuid::nil(),
        };
        assert!(should_skip_duplicate_input(Some(&input), &input, false));
        assert!(!should_skip_duplicate_input(Some(&input), &input, true));

        let mut changed = input.clone();
        changed.cursor += 1;
        assert!(!should_skip_duplicate_input(Some(&input), &changed, false));
    }

    #[test]
    fn hidden_overlay_rejects_delayed_row_actions() {
        assert!(!action_is_allowed("insertSelected", false, false));
        assert!(!action_is_allowed("navigateDown", false, false));
        assert!(!action_is_allowed("selectSuggestion1", false, false));
        assert!(action_is_allowed("showAutocompleteFromTab", false, false));
        assert!(action_is_allowed("toggleAutocomplete", false, false));
    }

    #[test]
    fn a_completion_releases_its_own_loading_marker() {
        assert!(loading_owner_is_released_by(7, 7));
    }

    #[test]
    fn a_completion_releases_a_marker_stranded_by_an_older_request() {
        // The old WebView derived its loading flag from the live generator set,
        // so it could not be stranded. Here generation 6 turned the marker on
        // and its result was dropped as stale; generation 7 has to clean up or
        // `···` stays on screen until the next keystroke.
        assert!(loading_owner_is_released_by(6, 7));
    }

    #[test]
    fn a_completion_leaves_a_newer_requests_marker_alone() {
        assert!(!loading_owner_is_released_by(8, 7));
    }

    #[test]
    fn nothing_to_release_when_no_request_owns_the_marker() {
        assert!(!loading_owner_is_released_by(NO_LOADING_OWNER, 7));
    }

    #[test]
    fn deleting_a_whole_token_reads_as_a_new_token() {
        assert!(backspaced_to_new_token("git commit -m", "git commit"));
        assert!(backspaced_to_new_token("git c", "git "));
    }

    #[test]
    fn ordinary_backspaces_within_a_token_keep_the_list_up() {
        assert!(!backspaced_to_new_token("git com", "git co"));
        assert!(!backspaced_to_new_token("git commit ", "git commit"));
        assert!(!backspaced_to_new_token("git", "gi"));
        // Typing forward never counts, even when it adds a token.
        assert!(!backspaced_to_new_token("git ", "git c"));
    }

    #[test]
    fn typing_one_character_is_not_a_large_buffer_change() {
        assert!(!large_buffer_change("git", "git "));
        assert!(!large_buffer_change("git ", "git"));
        assert!(!large_buffer_change("git", "git"));
    }

    #[test]
    fn pasting_or_recalling_history_is_a_large_buffer_change() {
        // Up-arrow at an empty prompt.
        assert!(large_buffer_change("", "git commit -m 'wip'"));
        // A paste that shares no prefix with what was there.
        assert!(large_buffer_change("git st", "npm run build"));
        // Deleting a word.
        assert!(large_buffer_change("git commit", "git "));
    }

    #[test]
    fn multibyte_edits_are_counted_in_characters_not_bytes() {
        // One deleted CJK character is three bytes but a single keystroke.
        assert!(!large_buffer_change("echo 中文", "echo 中"));
    }

    #[test]
    fn a_common_prefix_that_spells_out_the_row_is_a_full_acceptance() {
        // `getFullInsertion` omits the trailing space an acceptance appends,
        // so `should_add_space` must not break the comparison.
        let item = SuggestionItem {
            name: "checkout".into(),
            kind: "subcommand".into(),
            should_add_space: true,
            ..SuggestionItem::default()
        };
        assert!(prefix_completes_row("checkout", &item));
        assert!(!prefix_completes_row("check", &item));
    }

    #[test]
    fn a_common_prefix_never_triggers_an_auto_execute_row() {
        let item = SuggestionItem {
            name: "status".into(),
            kind: "auto-execute".into(),
            insert_value: Some("status".into()),
            ..SuggestionItem::default()
        };
        assert!(!prefix_completes_row("status", &item));
    }

    #[test]
    fn loading_overlay_rejects_actions_for_hidden_stale_rows() {
        assert!(!action_is_allowed("insertSelected", true, true));
        assert!(!action_is_allowed("insertCommonPrefix", true, true));
        assert!(!action_is_allowed("navigateDown", true, true));
        assert!(!action_is_allowed("showAutocompleteFromTab", true, true));
        assert!(action_is_allowed("hideAutocomplete", true, true));
    }

    #[test]
    fn click_must_match_the_visible_result_and_raw_search() {
        let item = SuggestionItem {
            name: "checkout".into(),
            description: "Switch branches".into(),
            kind: "subcommand".into(),
            insert_value: Some("checkout".into()),
            query_term: Some("co".into()),
            ..SuggestionItem::default()
        };
        let mut overlay = OverlayState::new();
        overlay.set_suggestions(vec![item.clone()], "'co".into());
        let mut click = ClickInsert {
            name: item.name,
            description: item.description,
            search: "'co".into(),
            kind: item.kind,
            args_hint: item.args_hint,
            insert_value: item.insert_value,
            display_name: item.display_name,
            primary_name: item.primary_name,
            separator_to_add: item.separator_to_add,
            should_add_space: item.should_add_space,
            hidden: item.hidden,
            priority: item.priority,
            icon_identifier: item.icon_identifier,
            original_type: item.original_type,
            query_term: item.query_term,
        };
        assert!(click_matches_current(&click, &overlay));
        click.search = "co".into();
        assert!(!click_matches_current(&click, &overlay));
        click.search = "'co".into();
        overlay.hide();
        assert!(!click_matches_current(&click, &overlay));

        overlay.visible = true;
        overlay.loading = true;
        assert!(!click_matches_current(&click, &overlay));
    }

    #[test]
    fn no_popout_clamps_to_the_right_edge_instead_of_shifting_by_popout_width() {
        let (x, _, on_left, _) = place_overlay_at_caret(
            350.0,
            100.0,
            16.0,
            328.0,
            80.0,
            140.0,
            Some((0.0, 0.0, 400.0, 800.0)),
            None,
            false,
        );
        assert!(!on_left);
        assert_eq!(x, 72.0);
    }

    #[test]
    fn popout_flips_to_the_left_when_the_combined_width_overflows() {
        let popout_shift = ec_gpui::POPOUT_WIDTH as f64;
        let width = 328.0 + popout_shift;
        let (x, _, on_left, _) = place_overlay_at_caret(
            400.0,
            100.0,
            16.0,
            width,
            80.0,
            140.0,
            Some((0.0, 0.0, 800.0, 800.0)),
            None,
            true,
        );
        assert!(on_left);
        assert_eq!(x, 400.0 - popout_shift);
    }

    #[test]
    fn no_overflow_keeps_the_overlay_at_the_caret() {
        let (x, y, on_left, is_above) = place_overlay_at_caret(
            40.0,
            80.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert_eq!(x, 40.0);
        assert_eq!(y, 80.0 + 16.0 + 2.0);
        assert!(!on_left);
        assert!(!is_above);
    }

    #[test]
    fn negative_secondary_screen_coordinates_are_preserved() {
        let (x, y, _, _) = place_overlay_at_caret(
            -1200.0,
            80.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((-1440.0, 0.0, 0.0, 900.0)),
            None,
            false,
        );
        assert_eq!(x, -1200.0);
        assert_eq!(y, 98.0);
    }

    /// 1440x900 laptop as the primary display with a 2560x1440 monitor to its
    /// right, bottom-aligned. In caret (Quartz) space the taller screen starts
    /// 540px above the primary's top edge.
    const DUAL_DISPLAY: [(f64, f64, f64, f64); 2] = [(0.0, 0.0, 1440.0, 900.0), (1440.0, -540.0, 2560.0, 1440.0)];

    #[test]
    fn a_caret_on_the_external_monitor_selects_that_monitor() {
        assert_eq!(
            screen_edges_containing(&DUAL_DISPLAY, 2000.0, -200.0),
            Some((1440.0, -540.0, 4000.0, 900.0))
        );
    }

    #[test]
    fn a_caret_on_the_primary_display_still_selects_the_primary() {
        assert_eq!(
            screen_edges_containing(&DUAL_DISPLAY, 400.0, 300.0),
            Some((0.0, 0.0, 1440.0, 900.0))
        );
    }

    #[test]
    fn an_external_monitor_caret_is_not_dragged_onto_the_primary() {
        // Above the primary's top edge and far to its right: clamping against
        // the primary would yank the overlay to y = 0 and x <= 1112.
        let edges = screen_edges_containing(&DUAL_DISPLAY, 2000.0, -200.0);
        let (x, y, _, is_above) = place_overlay_at_caret(2000.0, -200.0, 16.0, 328.0, 88.0, 140.0, edges, None, false);
        assert_eq!(x, 2000.0);
        assert_eq!(y, -182.0);
        assert!(!is_above);
    }

    #[test]
    fn a_2x_screen_flips_as_an_800px_window() {
        let scale = overlay_placement_scale_or_one(2.0);
        let (width, height, flip) = (400.0 * scale, 400.0 * scale, 140.0 * scale);
        assert_eq!((width, height, flip), (800.0, 800.0, 280.0));
        let (_, y, _, is_above) = place_overlay_at_caret(
            100.0,
            1000.0,
            16.0,
            width,
            height,
            flip,
            Some((0.0, 0.0, 1920.0, 1080.0)),
            None,
            false,
        );
        assert!(is_above);
        assert_eq!(y, 1000.0 - 800.0 - 2.0);
    }

    #[test]
    fn bottom_overflow_places_overlay_two_points_above_caret() {
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            560.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert_eq!(y, 560.0 - 88.0 - 2.0);
        assert!(is_above);
    }

    #[test]
    fn overlay_below_the_caret_is_clamped_onto_the_screen() {
        // A short flip budget keeps the overlay below the caret, but the window
        // itself must still fit: the WebView clamped it up to the screen edge.
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            560.0,
            16.0,
            328.0,
            88.0,
            10.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert!(!is_above);
        assert_eq!(y, 600.0 - 88.0);
    }

    #[test]
    fn terminal_bottom_only_decides_the_flip_and_never_clamps() {
        // Short terminal on a tall screen: flip above the caret, but keep the
        // window positioned against the screen rather than the terminal.
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            300.0,
            16.0,
            328.0,
            88.0,
            140.0,
            Some((0.0, 0.0, 800.0, 900.0)),
            Some(200.0),
            false,
        );
        assert!(is_above);
        assert_eq!(y, 300.0 - 88.0 - 2.0);
    }

    #[test]
    fn short_list_uses_configured_height_for_stable_flip_decision() {
        let (_, y, _, is_above) = place_overlay_at_caret(
            40.0,
            470.0,
            16.0,
            328.0,
            28.0,
            140.0,
            Some((0.0, 0.0, 800.0, 600.0)),
            None,
            false,
        );
        assert_eq!(y, 470.0 - 28.0 - 2.0);
        assert!(is_above);
    }

    #[test]
    fn ime_bottom_left_caret_is_converted_to_screen_top_left() {
        assert_eq!(
            caret_y_in_screen_space(120.0, 18.0, Origin::BottomLeft, Some((0.0, 0.0, 1440.0, 900.0)),),
            762.0
        );
        assert_eq!(
            caret_y_in_screen_space(120.0, 18.0, Origin::TopLeft, Some((0.0, 0.0, 1440.0, 900.0))),
            120.0
        );
        // Cocoa coordinates remain global across vertically arranged displays:
        // values above the primary screen map to negative top-left Y, while
        // values below it map past the primary screen height.
        assert_eq!(
            caret_y_in_screen_space(1_000.0, 18.0, Origin::BottomLeft, Some((0.0, 0.0, 1440.0, 900.0))),
            -118.0
        );
        assert_eq!(
            caret_y_in_screen_space(-200.0, 18.0, Origin::BottomLeft, Some((0.0, 0.0, 1440.0, 900.0))),
            1_082.0
        );
    }

    #[test]
    fn select_suggestion_ignores_out_of_range_indices() {
        assert_eq!(select_suggestion_index(1, 2), Some(0));
        assert_eq!(select_suggestion_index(2, 2), Some(1));
        assert_eq!(select_suggestion_index(5, 2), None);
        assert_eq!(select_suggestion_index(0, 2), None);
        assert_eq!(select_suggestion_index(1, 0), None);
        assert_eq!(select_suggestion_index(10, 10), Some(9));
    }

    #[test]
    fn user_keybindings_are_appended_after_defaults() {
        let user = KeyBindings(vec![
            KeyBinding {
                identifier: "increaseSize".into(),
                binding: "control+=".into(),
            },
            KeyBinding {
                identifier: "insertSelected".into(),
                binding: "tab".into(),
            },
        ]);
        let actions = merge_overlay_actions(DEFAULT_OVERLAY_BINDINGS, user);
        assert!(
            actions
                .iter()
                .any(|action| action.identifier == "insertSelected" && action.bindings.iter().any(|b| b == "enter"))
        );
        assert!(
            actions
                .iter()
                .any(|action| { action.identifier == "increaseSize" && action.bindings == ["control+=".to_string()] })
        );
        let last = actions.last().expect("user binding appended");
        assert_eq!(last.identifier, "insertSelected");
        assert_eq!(last.bindings, vec!["tab".to_string()]);
    }

    #[test]
    fn description_hint_symbolizes_modifier_and_uses_unicode_keys() {
        assert_eq!(format_keybinding("command+i"), "⌘i");
        assert_eq!(format_keybinding("control+shift+k"), "⌃⇧k");
        assert_eq!(format_keybinding("option+return"), "⌥↵");
    }
}
