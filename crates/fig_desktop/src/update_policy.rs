//! Sparkle update policy compiled on every OS.
//!
//! `update.rs` is still `cfg(macos)` and talks to Sparkle / AppKit. Linux CI
//! pins the LSUIElement contracts: a scheduled reminder must present
//! immediately (the agent never becomes active), automatic checks are on
//! (the first-run prompt cannot surface), and auto-download is off (ad-hoc
//! signed + no installer launcher, so a silent install never finishes).
//! Not live Sparkle / DMG.

#![allow(dead_code)]

/// `standardUserDriverShouldHandleShowingScheduledUpdate:andInImmediateFocus:`
/// must return yes. Sparkle's default scheduled reminder waits for the app
/// to come to the foreground, which an `LSUIElement` never does.
pub fn sparkle_scheduled_update_presents_immediately() -> bool {
    true
}

/// The first-run "Check for updates automatically?" prompt cannot reliably
/// surface on a menu-bar agent, so we set the choice ourselves.
pub fn sparkle_automatically_checks_for_updates() -> bool {
    true
}

/// `SUAutomaticallyUpdate` left at YES from a prior install silently
/// downloads. Ad-hoc signed Easy Complete cannot finish that path, so the
/// alert never appears. Force a prompt.
pub fn sparkle_automatically_downloads_updates() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsui_element_sparkle_must_prompt_not_wait_or_silent_install() {
        assert!(sparkle_scheduled_update_presents_immediately());
        assert!(sparkle_automatically_checks_for_updates());
        assert!(!sparkle_automatically_downloads_updates());
        let src = include_str!("update.rs");
        assert!(
            src.contains("sparkle_scheduled_update_presents_immediately()"),
            "scheduled Sparkle reminder must use the shared LSUIElement policy"
        );
        assert!(
            src.contains("sparkle_automatically_checks_for_updates()")
                && src.contains("sparkle_automatically_downloads_updates()"),
            "Sparkle auto-check / auto-download must use the shared policy"
        );
        assert!(
            src.contains("setAutomaticallyChecksForUpdates: checks")
                && src.contains("setAutomaticallyDownloadsUpdates: downloads"),
            "Sparkle YES/NO flags must come from the shared policy, not literals on the msg_send"
        );
    }
}
