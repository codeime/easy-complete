use std::os::raw::c_void;

use core_foundation::array::CFArray;
use core_foundation::base::{CFGetTypeID, CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryGetTypeID};
use core_foundation::string::CFString;
use core_foundation_sys::preferences::{
    CFPreferencesAppSynchronize, CFPreferencesCopyAppValue, CFPreferencesSetAppValue,
};

const DOMAIN: &str = "com.apple.HIToolbox";
/// The list a new window is matched against when macOS hands it an IMK
/// connection. Missing from here means no caret, whatever the palette shows.
const ENABLED_KEY: &str = "AppleEnabledInputSources";
/// What the input-source palette shows as active.
const SELECTED_KEY: &str = "AppleSelectedInputSources";
const BUNDLE_ID_KEY: &str = "Bundle ID";
const KIND_KEY: &str = "InputSourceKind";
const NON_KEYBOARD_KIND: &str = "Non Keyboard Input Method";

/// Whether `AppleEnabledInputSources` names this source.
///
/// `AppleSelectedInputSources` is not a substitute. A palette can be selected
/// and disabled at the same time — that is the state a `TISDisableInputSource`
/// followed by `TISEnableInputSource` leaves behind — and in it every new
/// terminal window comes up without an IMK connection.
pub fn is_palette_enabled(bundle_id: &str) -> bool {
    domain_lists_palette(DOMAIN, ENABLED_KEY, bundle_id)
}

/// Put `bundle_id` in both palette lists, and return whether both now name it.
///
/// Other input sources are preserved in order. The only entry ever dropped is a
/// previous bundle ID of this same palette, which a vendor rename leaves behind
/// as a source pointing at a bundle that no longer exists.
pub fn ensure_palette_enabled(bundle_id: &str) -> bool {
    // Both keys, even when one is already right: they are read independently and
    // the enabled/selected split is exactly how the caret went missing.
    let enabled = ensure_listed(DOMAIN, ENABLED_KEY, bundle_id);
    let selected = ensure_listed(DOMAIN, SELECTED_KEY, bundle_id);
    enabled && selected
}

/// What an existing palette entry means for the source being installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// This exact source, already listed.
    Ours,
    /// The same palette under an older bundle ID, left by a rename.
    Superseded,
    /// Somebody else's input source. Never rewritten, never reordered.
    Other,
}

fn classify(bundle_id: Option<&str>, kind: Option<&str>, ours: &str, vendor_prefix: &str) -> Entry {
    if bundle_id == Some(ours) {
        return Entry::Ours;
    }
    // An empty prefix would match every palette on the machine, so a bundle ID
    // with too few components disables the rename cleanup rather than widening it.
    let same_vendor = !vendor_prefix.is_empty() && bundle_id.is_some_and(|id| id.starts_with(vendor_prefix));
    if same_vendor && kind == Some(NON_KEYBOARD_KIND) {
        Entry::Superseded
    } else {
        Entry::Other
    }
}

/// `dev.emmmm.easy-complete.inputmethod` → `dev.emmmm`: the reverse-DNS vendor,
/// which survives renaming both the app and the helper.
fn vendor_prefix(bundle_id: &str) -> &str {
    bundle_id
        .rsplit_once('.')
        .map_or("", |(head, _)| head)
        .rsplit_once('.')
        .map_or("", |(head, _)| head)
}

fn palette_entry(bundle_id: &str) -> CFDictionary<CFString, CFString> {
    CFDictionary::from_CFType_pairs(&[
        (CFString::new(BUNDLE_ID_KEY), CFString::new(bundle_id)),
        (CFString::new(KIND_KEY), CFString::new(NON_KEYBOARD_KIND)),
    ])
}

fn copy_list(domain: &str, key: &str) -> Option<CFArray> {
    let key = CFString::new(key);
    let domain = CFString::new(domain);

    // This process may have read the domain minutes ago and cached it; the IME
    // and the CLI both write it during an install. Synchronizing first drops
    // that cache, so a rewrite merges into what is actually on disk.
    unsafe { CFPreferencesAppSynchronize(domain.as_concrete_TypeRef()) };

    let value = unsafe { CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), domain.as_concrete_TypeRef()) };
    if value.is_null() {
        return None;
    }

    let value = unsafe { CFType::wrap_under_create_rule(value) };
    if !value.instance_of::<CFArray>() {
        return None;
    }
    Some(unsafe { CFArray::wrap_under_get_rule(value.as_CFTypeRef().cast()) })
}

/// `Bundle ID` and `InputSourceKind` off one entry. Anything that is not a
/// dictionary of strings reads as absent, which classifies it as somebody
/// else's and leaves it untouched.
fn entry_strings(item: *const c_void) -> (Option<String>, Option<String>) {
    if item.is_null() || unsafe { CFGetTypeID(item) } != unsafe { CFDictionaryGetTypeID() } {
        return (None, None);
    }

    let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(item.cast()) };
    (dict_string(&dict, BUNDLE_ID_KEY), dict_string(&dict, KIND_KEY))
}

fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    let key = CFString::new(key);
    dict.find(&key)?.downcast::<CFString>().map(|value| value.to_string())
}

fn domain_lists_palette(domain: &str, key: &str, bundle_id: &str) -> bool {
    let Some(list) = copy_list(domain, key) else {
        return false;
    };
    list.iter()
        .any(|item| entry_strings(*item).0.as_deref() == Some(bundle_id))
}

/// Rewrite one list so it names `bundle_id` exactly once, appended last.
/// A list that already does is left alone rather than rewritten in place.
fn ensure_listed(domain: &str, key: &str, bundle_id: &str) -> bool {
    let prefix = vendor_prefix(bundle_id);

    let mut others: Vec<CFType> = Vec::new();
    let mut ours = 0usize;
    let mut superseded = 0usize;

    if let Some(list) = copy_list(domain, key) {
        for item in list.iter() {
            let item: *const c_void = *item;
            let (entry_id, kind) = entry_strings(item);
            match classify(entry_id.as_deref(), kind.as_deref(), bundle_id, prefix) {
                Entry::Ours => ours += 1,
                Entry::Superseded => superseded += 1,
                Entry::Other => others.push(unsafe { CFType::wrap_under_get_rule(item) }),
            }
        }
    }

    if ours == 1 && superseded == 0 {
        return true;
    }

    others.push(palette_entry(bundle_id).as_CFType());
    write_list(domain, key, &CFArray::from_CFTypes(&others))
}

fn write_list(domain: &str, key: &str, list: &CFArray<CFType>) -> bool {
    let key = CFString::new(key);
    let domain = CFString::new(domain);
    unsafe {
        CFPreferencesSetAppValue(
            key.as_concrete_TypeRef(),
            list.as_CFTypeRef(),
            domain.as_concrete_TypeRef(),
        );
        CFPreferencesAppSynchronize(domain.as_concrete_TypeRef()) != 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// Never the real domain: a test must not touch the input sources of the
    /// machine it runs on.
    const TEST_DOMAIN: &str = "dev.emmmm.easy-complete.palette-test";
    const OURS: &str = "dev.emmmm.easy-complete.inputmethod";

    static SERIAL: Mutex<()> = Mutex::new(());

    /// Take the scratch domain for one test, or `None` when preferences are not
    /// writable here.
    ///
    /// CoreFoundation aborts the process when several threads drive the same
    /// preference domain at once, so these tests hold a lock. And a sandboxed
    /// runner has no cfprefsd: every write there returns false, which says
    /// nothing about the code, so skip rather than fail.
    fn scratch(key: &str) -> Option<MutexGuard<'static, ()>> {
        let guard = SERIAL.lock().unwrap_or_else(|err| err.into_inner());
        let empty: [CFType; 0] = [];
        let writable = write_list(TEST_DOMAIN, key, &CFArray::from_CFTypes(&empty));
        clear(key);
        if !writable {
            eprintln!("skipping: cfprefsd will not accept a write in this environment");
            return None;
        }
        Some(guard)
    }

    #[test]
    fn vendor_prefix_keeps_the_reverse_dns_vendor() {
        assert_eq!(vendor_prefix("dev.emmmm.easy-complete.inputmethod"), "dev.emmmm");
        assert_eq!(vendor_prefix("com.example.app"), "com");
        assert_eq!(vendor_prefix("two.parts"), "");
        assert_eq!(vendor_prefix("single"), "");
    }

    #[test]
    fn our_own_entry_is_recognised() {
        assert_eq!(
            classify(Some(OURS), Some(NON_KEYBOARD_KIND), OURS, "dev.emmmm"),
            Entry::Ours
        );
        // A stale entry keeps its identity even without the kind recorded.
        assert_eq!(classify(Some(OURS), None, OURS, "dev.emmmm"), Entry::Ours);
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
            Entry::Superseded
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
            Entry::Other
        );
        assert_eq!(
            classify(
                Some("com.sogou.inputmethod"),
                Some(NON_KEYBOARD_KIND),
                OURS,
                "dev.emmmm"
            ),
            Entry::Other
        );
        // Same vendor, but a keyboard layout rather than this palette.
        assert_eq!(
            classify(
                Some("dev.emmmm.keylayout.x"),
                Some("Keyboard Layout"),
                OURS,
                "dev.emmmm"
            ),
            Entry::Other
        );
        assert_eq!(classify(None, None, OURS, "dev.emmmm"), Entry::Other);
    }

    /// An unparsed bundle ID must not turn the rename cleanup into "drop every
    /// palette on the machine".
    #[test]
    fn an_empty_vendor_prefix_supersedes_nothing() {
        assert_eq!(
            classify(Some("com.sogou.inputmethod"), Some(NON_KEYBOARD_KIND), "single", ""),
            Entry::Other
        );
    }

    fn seed(key: &str, entries: &[(&str, &str)]) {
        let items: Vec<CFType> = entries
            .iter()
            .map(|(bundle_id, kind)| {
                CFDictionary::from_CFType_pairs(&[
                    (CFString::new(BUNDLE_ID_KEY), CFString::new(bundle_id)),
                    (CFString::new(KIND_KEY), CFString::new(kind)),
                ])
                .as_CFType()
            })
            .collect();
        assert!(write_list(TEST_DOMAIN, key, &CFArray::from_CFTypes(&items)));
    }

    /// Entries with whatever keys the caller names, for the shapes a real
    /// `AppleEnabledInputSources` actually holds.
    fn seed_raw(key: &str, entries: &[&[(&str, &str)]]) {
        let items: Vec<CFType> = entries
            .iter()
            .map(|pairs| {
                let pairs: Vec<(CFString, CFString)> = pairs
                    .iter()
                    .map(|(field, value)| (CFString::new(field), CFString::new(value)))
                    .collect();
                CFDictionary::from_CFType_pairs(&pairs).as_CFType()
            })
            .collect();
        assert!(write_list(TEST_DOMAIN, key, &CFArray::from_CFTypes(&items)));
    }

    fn value_at(key_name: &str, index: usize, field: &str) -> Option<String> {
        let list = copy_list(TEST_DOMAIN, key_name)?;
        let item: *const c_void = *list.iter().nth(index)?;
        if item.is_null() {
            return None;
        }
        let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(item.cast()) };
        dict_string(&dict, field)
    }

    fn read_back(key: &str) -> Vec<(Option<String>, Option<String>)> {
        copy_list(TEST_DOMAIN, key)
            .map(|list| list.iter().map(|item| entry_strings(*item)).collect())
            .unwrap_or_default()
    }

    /// A null value deletes the key, which is how the scratch domain is left
    /// clean whether or not the test that seeded it got that far.
    fn clear(key: &str) {
        let key = CFString::new(key);
        let domain = CFString::new(TEST_DOMAIN);
        unsafe {
            CFPreferencesSetAppValue(
                key.as_concrete_TypeRef(),
                std::ptr::null(),
                domain.as_concrete_TypeRef(),
            );
            CFPreferencesAppSynchronize(domain.as_concrete_TypeRef());
        }
    }

    /// The whole point of the change: the read-modify-write goes through
    /// CFPreferences in-process, with no `python3` and no `defaults` on `PATH`.
    #[test]
    fn a_rewrite_preserves_other_sources_and_appends_ours() {
        let key = "PaletteTestAppend";
        let Some(_serial) = scratch(key) else { return };
        seed(
            key,
            &[
                ("com.apple.keylayout.ABC", "Keyboard Layout"),
                ("dev.emmmm.old-name.inputmethod", NON_KEYBOARD_KIND),
            ],
        );

        assert!(ensure_listed(TEST_DOMAIN, key, OURS));

        let entries = read_back(key);
        let ids: Vec<Option<&str>> = entries.iter().map(|(id, _)| id.as_deref()).collect();
        assert_eq!(ids, [Some("com.apple.keylayout.ABC"), Some(OURS)]);
        assert_eq!(entries[1].1.as_deref(), Some(NON_KEYBOARD_KIND));

        clear(key);
    }

    /// A list that is already correct is reported as such without a write, so a
    /// restarted IME does not churn the domain on every launch.
    #[test]
    fn an_already_listed_palette_is_left_alone() {
        let key = "PaletteTestIdempotent";
        let Some(_serial) = scratch(key) else { return };
        seed(
            key,
            &[
                (OURS, NON_KEYBOARD_KIND),
                ("com.apple.keylayout.ABC", "Keyboard Layout"),
            ],
        );

        assert!(ensure_listed(TEST_DOMAIN, key, OURS));

        let ids: Vec<Option<String>> = read_back(key).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, [Some(OURS.to_owned()), Some("com.apple.keylayout.ABC".to_owned())]);

        clear(key);
    }

    /// The state that hid Otty's list: selected but not enabled. A missing key
    /// has to be created from nothing, not just appended to.
    #[test]
    fn a_missing_list_is_created() {
        let key = "PaletteTestMissing";
        let Some(_serial) = scratch(key) else { return };

        assert!(!domain_lists_palette(TEST_DOMAIN, key, OURS));
        assert!(ensure_listed(TEST_DOMAIN, key, OURS));
        assert!(domain_lists_palette(TEST_DOMAIN, key, OURS));

        clear(key);
    }

    /// A live `AppleEnabledInputSources` holds entries that are not bundle-and-
    /// kind pairs at all: a keyboard layout carries `KeyboardLayout ID` and no
    /// `Bundle ID`, an input mode carries `Input Mode`. Those belong to other
    /// input sources, so a rewrite has to hand them back with every key intact
    /// and in the same order — dropping one would disable somebody's keyboard.
    #[test]
    fn foreign_entries_survive_a_rewrite_with_every_key() {
        let key = "PaletteTestForeign";
        let Some(_serial) = scratch(key) else { return };
        seed_raw(
            key,
            &[
                &[
                    ("Bundle ID", "com.apple.PressAndHold"),
                    ("InputSourceKind", NON_KEYBOARD_KIND),
                ],
                &[
                    ("InputSourceKind", "Keyboard Layout"),
                    ("KeyboardLayout ID", "252"),
                    ("KeyboardLayout Name", "ABC"),
                ],
                &[
                    ("Bundle ID", "com.apple.inputmethod.SCIM"),
                    ("Input Mode", "com.apple.inputmethod.SCIM.ITABC"),
                    ("InputSourceKind", "Input Mode"),
                ],
            ],
        );

        assert!(ensure_listed(TEST_DOMAIN, key, OURS));

        let ids: Vec<Option<String>> = read_back(key).into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            ids,
            [
                Some("com.apple.PressAndHold".to_owned()),
                None,
                Some("com.apple.inputmethod.SCIM".to_owned()),
                Some(OURS.to_owned()),
            ]
        );
        assert_eq!(value_at(key, 1, "KeyboardLayout Name").as_deref(), Some("ABC"));
        assert_eq!(value_at(key, 1, "KeyboardLayout ID").as_deref(), Some("252"));
        assert_eq!(
            value_at(key, 2, "Input Mode").as_deref(),
            Some("com.apple.inputmethod.SCIM.ITABC")
        );

        clear(key);
    }

    /// A duplicate is collapsed rather than left to grow on every install.
    #[test]
    fn duplicates_collapse_to_one_entry() {
        let key = "PaletteTestDuplicate";
        let Some(_serial) = scratch(key) else { return };
        seed(key, &[(OURS, NON_KEYBOARD_KIND), (OURS, NON_KEYBOARD_KIND)]);

        assert!(ensure_listed(TEST_DOMAIN, key, OURS));

        let ids: Vec<Option<String>> = read_back(key).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, [Some(OURS.to_owned())]);

        clear(key);
    }
}
