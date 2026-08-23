//! macOS install / Accessibility-reconcile policy compiled on every OS.
//!
//! `install.rs` is still `cfg(macos)` for the AppKit prompt, `tccutil`, and
//! the Launch Services bits. Linux CI pins the contracts: the desktop always
//! restarts, the IME only when its hash changed, `tccutil reset` only when
//! the desktop binary changed, and a revoked Accessibility grant prompts once.
//!
//! Not live AX / IME / DMG / `tccutil`.

#![allow(dead_code)]

use semver::Version;

/// State-machine result for [`accessibility_reconcile_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityReconcileAction {
    /// TCC currently grants us. Record it so a later revocation is distinct
    /// from "never granted".
    RecordGrant,
    /// Was granted, now missing (reinstall invalidated TCC). Prompt once and
    /// clear the marker so the next launch is a tray warning, not another prompt.
    ClearGrantAndPrompt,
    /// Never granted, background/silent launch. Tray only.
    TrayOnly,
    /// Never granted, interactive launch. Leave to the once-per-version prompt.
    LeaveToLaunchPrompt,
}

pub fn accessibility_reconcile_action(
    currently_enabled: bool,
    previously_granted: bool,
    prompt_for_permissions: bool,
) -> AccessibilityReconcileAction {
    if currently_enabled {
        AccessibilityReconcileAction::RecordGrant
    } else if previously_granted {
        AccessibilityReconcileAction::ClearGrantAndPrompt
    } else if !prompt_for_permissions {
        AccessibilityReconcileAction::TrayOnly
    } else {
        AccessibilityReconcileAction::LeaveToLaunchPrompt
    }
}

/// `ClearGrantAndPrompt` already raised the System Settings prompt, so the
/// once-per-version install script must not raise a second one.
pub fn accessibility_reconcile_already_prompted(action: AccessibilityReconcileAction) -> bool {
    action == AccessibilityReconcileAction::ClearGrantAndPrompt
}

/// First launch always runs the once-per-version install. Debug builds skip
/// it after a version is stored so cargo-run does not re-prompt every start.
pub fn should_run_once_per_version_install(
    is_debug_build: bool,
    current: &Version,
    previous: Option<&Version>,
) -> bool {
    match previous {
        None => true,
        Some(prev) => !is_debug_build && current > prev,
    }
}

/// Desktop always restarts: `Contents/Resources/specs-ir` is read lazily, and
/// a live process pointed at a wiped bundle silently loses completions. It
/// holds no IMK connections worth preserving.
pub fn install_stops_desktop(_binary_changed: bool) -> bool {
    true
}

/// IME is replaced only when the helper bytes changed. Open Otty / Ghostty /
/// Kitty windows hold IMK connections that macOS never re-attaches.
pub fn install_stops_ime(binary_changed: bool) -> bool {
    binary_changed
}

/// Same desktop binary keeps its ad-hoc signature and therefore the existing
/// Accessibility grant. A new binary needs `tccutil reset`.
pub fn install_resets_accessibility(desktop_binary_changed: bool) -> bool {
    desktop_binary_changed
}

/// `ditto` merges. When the IME stays up, keep `Contents/Helpers` so the live
/// process is not unmapped.
pub fn install_keeps_helpers_directory(ime_running: bool, ime_binary_changed: bool) -> bool {
    ime_running && !ime_binary_changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessibility_revocation_prompts_once_then_falls_back_to_the_tray() {
        assert_eq!(
            accessibility_reconcile_action(true, false, false),
            AccessibilityReconcileAction::RecordGrant
        );
        assert_eq!(
            accessibility_reconcile_action(true, true, true),
            AccessibilityReconcileAction::RecordGrant
        );
        assert_eq!(
            accessibility_reconcile_action(false, true, true),
            AccessibilityReconcileAction::ClearGrantAndPrompt
        );
        assert_eq!(
            accessibility_reconcile_action(false, true, false),
            AccessibilityReconcileAction::ClearGrantAndPrompt
        );
        assert_eq!(
            accessibility_reconcile_action(false, false, false),
            AccessibilityReconcileAction::TrayOnly
        );
        assert_eq!(
            accessibility_reconcile_action(false, false, true),
            AccessibilityReconcileAction::LeaveToLaunchPrompt
        );
        assert!(accessibility_reconcile_already_prompted(
            AccessibilityReconcileAction::ClearGrantAndPrompt
        ));
        assert!(!accessibility_reconcile_already_prompted(
            AccessibilityReconcileAction::LeaveToLaunchPrompt
        ));
        let src = include_str!("install.rs");
        assert!(
            src.contains("accessibility_reconcile_action") && src.contains("accessibility_reconcile_already_prompted"),
            "launch-time Accessibility reconcile must use the shared state machine"
        );
        assert!(
            !src.contains("if previously_granted {") && !src.contains("if previously_granted\n"),
            "do not fork the granted→revoked prompt back into install.rs"
        );
    }

    #[test]
    fn once_per_version_install_runs_on_first_launch_and_skips_debug() {
        let v1 = Version::parse("2.2.2").unwrap();
        let v2 = Version::parse("2.2.3").unwrap();
        assert!(should_run_once_per_version_install(false, &v1, None));
        assert!(should_run_once_per_version_install(true, &v1, None));
        assert!(should_run_once_per_version_install(false, &v2, Some(&v1)));
        assert!(!should_run_once_per_version_install(true, &v2, Some(&v1)));
        assert!(!should_run_once_per_version_install(false, &v1, Some(&v1)));
        assert!(!should_run_once_per_version_install(false, &v1, Some(&v2)));
        let src = include_str!("install.rs");
        assert!(
            src.contains("should_run_once_per_version_install"),
            "macOS launch must use the shared once-per-version gate"
        );
    }

    #[test]
    fn install_sh_restarts_desktop_always_and_ime_only_on_hash_change() {
        assert!(install_stops_desktop(false));
        assert!(install_stops_desktop(true));
        assert!(install_stops_ime(true));
        assert!(!install_stops_ime(false));
        assert!(install_resets_accessibility(true));
        assert!(!install_resets_accessibility(false));
        assert!(install_keeps_helpers_directory(true, false));
        assert!(!install_keeps_helpers_directory(false, false));
        assert!(!install_keeps_helpers_directory(true, true));
        let sh = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/install.sh"));
        assert!(
            sh.contains("stop_process \"${APP_NAME}\"") && sh.contains("The desktop app always goes down"),
            "install.sh must stop the desktop even when its hash matches"
        );
        assert!(
            sh.contains("if [ \"${ime_changed}\" -eq 1 ]; then")
                && sh.contains("stop_process fig_input_method")
                && sh.contains("keep_ime=1"),
            "install.sh replaces the IME only when the helper hash changed"
        );
        assert!(
            sh.contains("if [ \"${desktop_changed}\" -eq 1 ]; then") && sh.contains("tccutil reset Accessibility"),
            "tccutil reset is gated on the desktop hash, not the IME hash"
        );
        assert!(
            sh.contains("keep_ime") && sh.contains("! -name Helpers"),
            "same-hash IME keep must leave Contents/Helpers in place for ditto"
        );
        assert!(
            !sh.contains("TISDisableInputSource") && !sh.contains("restart your terminal"),
            "install.sh must not bounce the input source or tell the user to restart terminals"
        );
    }
}
