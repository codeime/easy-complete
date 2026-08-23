//! When to replace the macOS IME process.
//!
//! `input_method` is `cfg(macos)` and talks to AppKit / TIS. This module is
//! compiled on every OS so Linux CI pins the contract: a missing hash tracker
//! is not stale (that would undo `install.sh` when the bytes already match),
//! and a running helper is only replaced when the on-disk binary changed.
//!
//! Live SIGTERM / `open` / IMK reconnect still needs a macOS host.

#![allow(dead_code)]

use std::time::Duration;

/// SIGTERM wait before SIGKILL. `open` on a live bundle only activates the
/// old process, so the replacement must not launch until this wait (and the
/// kill wait below) has actually emptied the PID list.
pub const IME_SIGTERM_WAIT: Duration = Duration::from_millis(800);
/// SIGKILL wait after the helper ignored SIGTERM.
pub const IME_SIGKILL_WAIT: Duration = Duration::from_millis(400);

/// What [`ime_launch_action`] tells the installer to do with the running helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeLaunchAction {
    /// No process. Start this bundle and record its hash.
    Start,
    /// Running process is a different binary. Stop it, then start this one.
    Replace,
    /// Running, but we never recorded a hash. Pin it; do not kill.
    PinHash,
    /// Running and the hash matches. Leave the IMK connections alone.
    Keep,
}

/// `launched` is the hash we recorded when we last started the IME; `disk` is
/// the hash of the bundle we want to run. Missing tracker state is *not* stale:
/// killing on that would undo `install.sh` when it already decided the bytes
/// match. The caller pins the hash instead. Sparkle after this tracker has
/// been written still sees `Some(old) != Some(new)` and replaces.
pub fn process_is_stale(launched: Option<&str>, disk: Option<&str>) -> bool {
    match (launched, disk) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

pub fn ime_launch_action(running: bool, launched_hash: Option<&str>, disk_hash: Option<&str>) -> ImeLaunchAction {
    if !running {
        return ImeLaunchAction::Start;
    }
    if process_is_stale(launched_hash, disk_hash) {
        return ImeLaunchAction::Replace;
    }
    if launched_hash.is_none() {
        ImeLaunchAction::PinHash
    } else {
        ImeLaunchAction::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_is_stale_only_when_the_hash_changed() {
        assert!(!process_is_stale(None, None));
        assert!(!process_is_stale(Some("aaa"), None));
        assert!(!process_is_stale(None, Some("aaa")));
        assert!(!process_is_stale(Some("aaa"), Some("aaa")));
        assert!(process_is_stale(Some("aaa"), Some("bbb")));
    }

    #[test]
    fn a_down_helper_is_started() {
        assert_eq!(ime_launch_action(false, None, Some("aaa")), ImeLaunchAction::Start);
        assert_eq!(
            ime_launch_action(false, Some("old"), Some("new")),
            ImeLaunchAction::Start
        );
    }

    #[test]
    fn a_changed_binary_replaces_the_running_process() {
        assert_eq!(
            ime_launch_action(true, Some("aaa"), Some("bbb")),
            ImeLaunchAction::Replace
        );
    }

    #[test]
    fn a_missing_tracker_is_not_a_reason_to_kill() {
        assert_eq!(ime_launch_action(true, None, Some("aaa")), ImeLaunchAction::PinHash);
        assert_eq!(ime_launch_action(true, None, None), ImeLaunchAction::PinHash);
    }

    #[test]
    fn a_matching_hash_keeps_imk_connections() {
        assert_eq!(ime_launch_action(true, Some("aaa"), Some("aaa")), ImeLaunchAction::Keep);
    }

    #[test]
    fn replace_waits_for_sigterm_then_sigkill_before_open() {
        assert_eq!(IME_SIGTERM_WAIT, Duration::from_millis(800));
        assert_eq!(IME_SIGKILL_WAIT, Duration::from_millis(400));
        let src = include_str!("input_method/mod.rs");
        assert!(
            src.contains("IME_SIGTERM_WAIT") && src.contains("IME_SIGKILL_WAIT"),
            "IME stop must wait through the shared SIGTERM/SIGKILL budgets"
        );
        assert!(
            !src.contains("Duration::from_millis(800)") && !src.contains("Duration::from_millis(400)"),
            "do not fork the IME stop waits back into the AppKit module"
        );
    }

    #[test]
    fn macos_host_uses_the_shared_launch_action() {
        let src = include_str!("input_method/mod.rs");
        assert!(
            src.contains("ime_launch_action("),
            "ensure_current_binary_running must go through the shared launch policy"
        );
        assert!(
            !src.contains("fn process_is_stale"),
            "do not fork the missing-tracker rule back into the AppKit module"
        );
    }

    #[test]
    fn ime_directory_does_not_unwrap_home_dir() {
        let production = include_str!("input_method/mod.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        let compact: String = production.split_whitespace().collect();
        assert!(
            !compact.contains("home_dir().unwrap()"),
            "a missing HOME must not panic IME install"
        );
        assert!(
            production.contains("input_method_directory()") && production.contains("InvalidDestination"),
            "the Input Methods path still lives under ~/Library"
        );
    }
}
