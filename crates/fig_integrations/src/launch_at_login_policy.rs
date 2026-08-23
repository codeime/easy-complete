//! Launch-at-login policy compiled on every OS.
//!
//! Live `SMAppService` / LaunchAgent / `HKCU\...\Run` writes stay in the OS
//! modules. This crate pins the contracts Linux CI can check: which
//! ServiceManagement status counts as enabled, when register/unregister is a
//! no-op, the two historical LaunchAgent labels, and Windows Run-key
//! NotFound handling. Not live login-item / registry I/O.

#![allow(dead_code)]

use std::io::ErrorKind;

/// `SMAppServiceStatus` from ServiceManagement.framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmAppServiceStatus {
    NotRegistered = 0,
    Enabled = 1,
    RequiresApproval = 2,
    NotFound = 3,
}

impl SmAppServiceStatus {
    pub fn from_raw(value: isize) -> Self {
        match value {
            1 => Self::Enabled,
            2 => Self::RequiresApproval,
            3 => Self::NotFound,
            _ => Self::NotRegistered,
        }
    }
}

/// `is_enabled` only follows Enabled. RequiresApproval is visible in System
/// Settings but is not a granted login item.
pub fn sm_app_service_counts_as_enabled(status: SmAppServiceStatus) -> bool {
    status == SmAppServiceStatus::Enabled
}

/// Skip `register`/`unregister` when the service is already in the desired
/// state. RequiresApproval still needs a call (the user has not granted it,
/// and disable must still unregister).
pub fn sm_app_service_already_in_desired_state(status: SmAppServiceStatus, enabled: bool) -> bool {
    if enabled {
        status == SmAppServiceStatus::Enabled
    } else {
        matches!(status, SmAppServiceStatus::NotRegistered | SmAppServiceStatus::NotFound)
    }
}

/// Current product LaunchAgent label (macOS 12 fallback).
pub const LEGACY_LAUNCH_AGENT_LABEL: &str = "dev.emmmm.easy-complete";
/// Amazon Q / CodeWhisperer launcher left behind on upgrade.
pub const UPSTREAM_LEGACY_LAUNCH_AGENT_LABEL: &str = "com.amazon.codewhisperer.launcher";

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub const WIN32_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Value name written under [`WIN32_RUN_KEY`].
pub const WIN32_RUN_VALUE: &str = "EasyComplete";

/// `RegKey::delete_value` on a missing name is success: the user already has
/// launch-at-login off.
pub fn win32_run_delete_not_found_is_ok(kind: ErrorKind) -> bool {
    kind == ErrorKind::NotFound
}

/// Opening the Run key can fail on a stripped-down profile; treat that as off,
/// not an error.
pub fn win32_run_missing_key_means_disabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use fig_util::consts::APP_BUNDLE_ID;

    #[test]
    fn sm_app_service_status_matches_service_management() {
        assert_eq!(SmAppServiceStatus::from_raw(0), SmAppServiceStatus::NotRegistered);
        assert_eq!(SmAppServiceStatus::from_raw(1), SmAppServiceStatus::Enabled);
        assert_eq!(SmAppServiceStatus::from_raw(2), SmAppServiceStatus::RequiresApproval);
        assert_eq!(SmAppServiceStatus::from_raw(3), SmAppServiceStatus::NotFound);
        assert_eq!(SmAppServiceStatus::from_raw(99), SmAppServiceStatus::NotRegistered);
        assert!(sm_app_service_counts_as_enabled(SmAppServiceStatus::Enabled));
        assert!(!sm_app_service_counts_as_enabled(SmAppServiceStatus::RequiresApproval));
        assert!(!sm_app_service_counts_as_enabled(SmAppServiceStatus::NotRegistered));
        assert!(!sm_app_service_counts_as_enabled(SmAppServiceStatus::NotFound));
    }

    #[test]
    fn sm_app_service_skips_register_and_unregister_when_already_there() {
        assert!(sm_app_service_already_in_desired_state(
            SmAppServiceStatus::Enabled,
            true
        ));
        assert!(!sm_app_service_already_in_desired_state(
            SmAppServiceStatus::RequiresApproval,
            true
        ));
        assert!(!sm_app_service_already_in_desired_state(
            SmAppServiceStatus::NotRegistered,
            true
        ));
        assert!(sm_app_service_already_in_desired_state(
            SmAppServiceStatus::NotRegistered,
            false
        ));
        assert!(sm_app_service_already_in_desired_state(
            SmAppServiceStatus::NotFound,
            false
        ));
        assert!(!sm_app_service_already_in_desired_state(
            SmAppServiceStatus::Enabled,
            false
        ));
        assert!(!sm_app_service_already_in_desired_state(
            SmAppServiceStatus::RequiresApproval,
            false
        ));
    }

    #[test]
    fn legacy_launch_agent_labels_cover_both_previous_install_paths() {
        assert_eq!(LEGACY_LAUNCH_AGENT_LABEL, APP_BUNDLE_ID);
        assert_ne!(LEGACY_LAUNCH_AGENT_LABEL, UPSTREAM_LEGACY_LAUNCH_AGENT_LABEL);
        assert_eq!(UPSTREAM_LEGACY_LAUNCH_AGENT_LABEL, "com.amazon.codewhisperer.launcher");
    }

    #[test]
    fn win32_run_key_delete_not_found_is_success() {
        assert_eq!(WIN32_RUN_VALUE, "EasyComplete");
        assert!(WIN32_RUN_KEY.contains(r"Windows\CurrentVersion\Run"));
        assert!(win32_run_delete_not_found_is_ok(ErrorKind::NotFound));
        assert!(!win32_run_delete_not_found_is_ok(ErrorKind::PermissionDenied));
        assert!(win32_run_missing_key_means_disabled());
    }

    #[test]
    fn macos_login_item_uses_shared_status_policy() {
        let src = include_str!("login_item.rs");
        assert!(
            src.contains("sm_app_service_already_in_desired_state"),
            "register/unregister no-ops must use the shared desired-state check"
        );
        assert!(
            src.contains("sm_app_service_counts_as_enabled"),
            "is_enabled must not treat RequiresApproval as granted"
        );
        assert!(
            src.contains("LEGACY_LAUNCH_AGENT_LABEL") && src.contains("UPSTREAM_LEGACY_LAUNCH_AGENT_LABEL"),
            "LaunchAgent cleanup must use the shared labels"
        );
        assert!(
            !src.contains("1 => Self::Enabled"),
            "do not keep a second SMAppService status table in login_item.rs"
        );
    }

    #[test]
    fn windows_run_key_uses_shared_policy() {
        let src = include_str!("launch_at_login.rs");
        assert!(
            src.contains("WIN32_RUN_KEY") && src.contains("WIN32_RUN_VALUE"),
            "HKCU Run writes must use the shared key/value"
        );
        assert!(
            src.contains("win32_run_delete_not_found_is_ok"),
            "delete of a missing Run value must use the shared NotFound check"
        );
        assert!(
            !src.contains("const RUN_KEY:") && !src.contains("const RUN_VALUE:"),
            "do not keep a second Run key table in launch_at_login.rs"
        );
    }
}
