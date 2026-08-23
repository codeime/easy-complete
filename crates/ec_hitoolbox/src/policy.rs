//! HIToolbox palette rewrite policy.
//!
//! `macos.rs` is `cfg(macos)` and talks to CoreFoundation. This module is
//! compiled on every OS so Linux CI pins the things that would otherwise only
//! run on a Mac: the reverse-DNS vendor prefix, the Ours/Superseded/Other
//! classification, and "an unparsable bundle ID must not match every palette".
//!
//! Live `CFPreferences` reads and writes still need a macOS host (and skip
//! when cfprefsd refuses a write).

#![allow(dead_code)]

/// `InputSourceKind` for a palette IM, which is what this helper is.
pub const NON_KEYBOARD_KIND: &str = "Non Keyboard Input Method";

/// What an existing palette entry means for the source being installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteEntry {
    /// This exact source, already listed.
    Ours,
    /// The same palette under an older bundle ID, left by a rename.
    Superseded,
    /// Somebody else's input source. Never rewritten, never reordered.
    Other,
}

/// Scan of one HIToolbox list against the source being installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaletteScan {
    pub ours: usize,
    pub superseded: usize,
    pub others: usize,
}

impl PaletteScan {
    /// A list that already names us exactly once and has no leftover rename
    /// entries is left alone rather than rewritten in place.
    pub fn leave_alone(self) -> bool {
        self.ours == 1 && self.superseded == 0
    }
}

/// `dev.emmmm.easy-complete.inputmethod` → `dev.emmmm`: the reverse-DNS vendor,
/// which survives renaming both the app and the helper.
pub fn vendor_prefix(bundle_id: &str) -> &str {
    bundle_id
        .rsplit_once('.')
        .map_or("", |(head, _)| head)
        .rsplit_once('.')
        .map_or("", |(head, _)| head)
}

pub fn classify(bundle_id: Option<&str>, kind: Option<&str>, ours: &str, vendor_prefix: &str) -> PaletteEntry {
    if bundle_id == Some(ours) {
        return PaletteEntry::Ours;
    }
    // An empty prefix would match every palette on the machine, so a bundle ID
    // with too few components disables the rename cleanup rather than widening it.
    let same_vendor = !vendor_prefix.is_empty() && bundle_id.is_some_and(|id| id.starts_with(vendor_prefix));
    if same_vendor && kind == Some(NON_KEYBOARD_KIND) {
        PaletteEntry::Superseded
    } else {
        PaletteEntry::Other
    }
}

pub fn scan_palette<'a, I>(entries: I, ours: &str) -> PaletteScan
where
    I: IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>,
{
    let prefix = vendor_prefix(ours);
    let mut scan = PaletteScan::default();
    for (id, kind) in entries {
        match classify(id, kind, ours, prefix) {
            PaletteEntry::Ours => scan.ours += 1,
            PaletteEntry::Superseded => scan.superseded += 1,
            PaletteEntry::Other => scan.others += 1,
        }
    }
    scan
}

/// Other sources in original order, then ours last. Superseded rename leftovers
/// and duplicate copies of ours are dropped. Bundle-less foreign entries keep
/// their slot as an empty id so a rewrite still preserves them.
pub fn rewritten_palette_ids<'a, I>(entries: I, ours: &str) -> Vec<String>
where
    I: IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>,
{
    let prefix = vendor_prefix(ours);
    let mut ids = Vec::new();
    for (id, kind) in entries {
        if classify(id, kind, ours, prefix) == PaletteEntry::Other {
            ids.push(id.unwrap_or("").to_string());
        }
    }
    ids.push(ours.to_string());
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "dev.emmmm.easy-complete.inputmethod";

    #[test]
    fn vendor_prefix_keeps_the_reverse_dns_vendor() {
        assert_eq!(vendor_prefix("dev.emmmm.easy-complete.inputmethod"), "dev.emmmm");
        assert_eq!(vendor_prefix("com.example.app"), "com");
        assert_eq!(vendor_prefix("two.parts"), "");
        assert_eq!(vendor_prefix("single"), "");
        assert_eq!(vendor_prefix(""), "");
    }

    #[test]
    fn our_own_entry_is_recognised() {
        assert_eq!(
            classify(Some(OURS), Some(NON_KEYBOARD_KIND), OURS, "dev.emmmm"),
            PaletteEntry::Ours
        );
        // A stale entry keeps its identity even without the kind recorded.
        assert_eq!(classify(Some(OURS), None, OURS, "dev.emmmm"), PaletteEntry::Ours);
    }

    #[test]
    fn a_renamed_copy_of_this_palette_is_superseded() {
        assert_eq!(
            classify(
                Some("dev.emmmm.old-name.inputmethod"),
                Some(NON_KEYBOARD_KIND),
                OURS,
                "dev.emmmm"
            ),
            PaletteEntry::Superseded
        );
    }

    #[test]
    fn other_vendors_and_keyboard_layouts_are_left_alone() {
        assert_eq!(
            classify(
                Some("com.apple.keylayout.ABC"),
                Some("Keyboard Layout"),
                OURS,
                "dev.emmmm"
            ),
            PaletteEntry::Other
        );
        assert_eq!(
            classify(
                Some("com.sogou.inputmethod"),
                Some(NON_KEYBOARD_KIND),
                OURS,
                "dev.emmmm"
            ),
            PaletteEntry::Other
        );
        // Same vendor, but a keyboard layout rather than this palette.
        assert_eq!(
            classify(
                Some("dev.emmmm.keylayout.x"),
                Some("Keyboard Layout"),
                OURS,
                "dev.emmmm"
            ),
            PaletteEntry::Other
        );
        assert_eq!(classify(None, None, OURS, "dev.emmmm"), PaletteEntry::Other);
    }

    /// An unparsed bundle ID must not turn the rename cleanup into "drop every
    /// palette on the machine".
    #[test]
    fn an_empty_vendor_prefix_supersedes_nothing() {
        assert_eq!(
            classify(Some("com.sogou.inputmethod"), Some(NON_KEYBOARD_KIND), "single", ""),
            PaletteEntry::Other
        );
        let scan = scan_palette([(Some("com.sogou.inputmethod"), Some(NON_KEYBOARD_KIND))], "single");
        assert_eq!(scan.superseded, 0);
        assert_eq!(scan.others, 1);
        assert_eq!(
            rewritten_palette_ids([(Some("com.sogou.inputmethod"), Some(NON_KEYBOARD_KIND))], "single"),
            ["com.sogou.inputmethod", "single"]
        );
    }

    #[test]
    fn rewrite_drops_superseded_and_appends_ours() {
        let entries = [
            (Some("com.apple.keylayout.ABC"), Some("Keyboard Layout")),
            (Some("dev.emmmm.old-name.inputmethod"), Some(NON_KEYBOARD_KIND)),
        ];
        assert!(!scan_palette(entries, OURS).leave_alone());
        assert_eq!(rewritten_palette_ids(entries, OURS), ["com.apple.keylayout.ABC", OURS]);
    }

    #[test]
    fn an_already_listed_palette_is_left_alone() {
        let entries = [
            (Some(OURS), Some(NON_KEYBOARD_KIND)),
            (Some("com.apple.keylayout.ABC"), Some("Keyboard Layout")),
        ];
        assert!(scan_palette(entries, OURS).leave_alone());
    }

    #[test]
    fn a_missing_list_is_just_ours() {
        let entries: [(Option<&str>, Option<&str>); 0] = [];
        assert!(!scan_palette(entries, OURS).leave_alone());
        assert_eq!(rewritten_palette_ids(entries, OURS), [OURS]);
    }

    #[test]
    fn duplicates_collapse_to_one_entry() {
        let entries = [
            (Some(OURS), Some(NON_KEYBOARD_KIND)),
            (Some(OURS), Some(NON_KEYBOARD_KIND)),
        ];
        assert!(!scan_palette(entries, OURS).leave_alone());
        assert_eq!(rewritten_palette_ids(entries, OURS), [OURS]);
    }

    #[test]
    fn bundleless_foreign_entries_keep_their_slot() {
        let entries = [
            (Some("com.apple.PressAndHold"), Some(NON_KEYBOARD_KIND)),
            (None, Some("Keyboard Layout")),
            (Some("com.apple.inputmethod.SCIM"), Some("Input Mode")),
        ];
        assert_eq!(
            rewritten_palette_ids(entries, OURS),
            ["com.apple.PressAndHold", "", "com.apple.inputmethod.SCIM", OURS]
        );
    }

    #[test]
    fn macos_host_classifies_through_this_module() {
        let src = include_str!("macos.rs");
        assert!(
            src.contains("crate::policy::"),
            "CFPreferences rewrite must call the shared classifier"
        );
        assert!(
            !src.contains("fn vendor_prefix") && !src.contains("fn classify("),
            "do not fork vendor_prefix / classify back into macos.rs"
        );
        assert!(
            src.contains("leave_alone()"),
            "an already-correct list still skips the write"
        );
    }
}
