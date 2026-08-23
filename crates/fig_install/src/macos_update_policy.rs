//! macOS Sparkle/DMG update layout policy compiled on every OS.
//!
//! `macos.rs` is still `cfg(macos)` and shells out to `hdiutil` / `ditto`.
//! Linux CI pins: hash mismatch fails, a missing `mount-point` is an error
//! not a panic, and the DMG must contain exactly one `.app`. Not live DMG.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

/// `hdiutil attach -plist` prints the mount-point as a string under that key.
pub fn hdiutil_mount_point(plist: &str) -> Option<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"<key>mount-point</key>\s*<\S+>([^<]+)</\S+>").expect("static mount-point regex"));
    re.captures(plist).and_then(|caps| caps.get(1)).map(|m| m.as_str())
}

pub fn dmg_hash_matches(expected: &str, actual: &str) -> bool {
    expected == actual
}

/// Directory entries whose path looks like a `.app` bundle (the live caller
/// also requires `is_dir()`).
pub fn is_app_bundle_name(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("app"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmgAppLayout {
    One,
    Missing,
    Multiple(usize),
}

pub fn dmg_app_layout(app_count: usize) -> DmgAppLayout {
    match app_count {
        1 => DmgAppLayout::One,
        0 => DmgAppLayout::Missing,
        n => DmgAppLayout::Multiple(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn hdiutil_mount_point_reads_the_plist_string_and_does_not_panic() {
        let plist = r#"
            <dict>
                <key>mount-point</key>
                <string>/Volumes/Easy Complete</string>
            </dict>
        "#;
        assert_eq!(hdiutil_mount_point(plist), Some("/Volumes/Easy Complete"));
        assert_eq!(hdiutil_mount_point("<dict></dict>"), None);
        assert_eq!(hdiutil_mount_point(""), None);
    }

    #[test]
    fn dmg_hash_mismatch_is_not_success() {
        assert!(dmg_hash_matches("abc", "abc"));
        assert!(!dmg_hash_matches("abc", "def"));
    }

    #[test]
    fn dmg_must_contain_exactly_one_app() {
        assert_eq!(dmg_app_layout(1), DmgAppLayout::One);
        assert_eq!(dmg_app_layout(0), DmgAppLayout::Missing);
        assert_eq!(dmg_app_layout(2), DmgAppLayout::Multiple(2));
        assert!(is_app_bundle_name(Path::new("Easy Complete.app")));
        assert!(is_app_bundle_name(Path::new("/Volumes/update/Easy Complete.app")));
        assert!(!is_app_bundle_name(Path::new("README.txt")));
        assert!(!is_app_bundle_name(Path::new("Applications")));
    }

    #[test]
    fn macos_update_uses_the_shared_dmg_policy() {
        let src = include_str!("macos.rs");
        assert!(
            src.contains("hdiutil_mount_point") && src.contains("dmg_hash_matches") && src.contains("dmg_app_layout"),
            "hdiutil attach / hash / .app count must go through the shared policy"
        );
        assert!(
            !src.contains(".captures(&plist).unwrap()") && !src.contains("expect(\"mount-point will always exist\")"),
            "a missing mount-point is UpdateFailed, not a panic"
        );
        assert!(
            src.contains("is_app_bundle_name") || src.contains("dmg_app_layout"),
            "exactly-one-.app must use the shared layout enum"
        );
    }
}
