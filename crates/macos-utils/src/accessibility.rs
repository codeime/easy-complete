use accessibility_sys::{AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt};
use core_foundation::base::TCFType;
use core_foundation::boolean::{CFBoolean, kCFBooleanTrue};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use objc2::ClassType;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

pub use crate::accessibility_guide::{accessibility_guide_is_active, begin_accessibility_guide};

static ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility";
static ACCESSIBILITY_SETTINGS_URL_LEGACY: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

pub fn open_accessibility() -> bool {
    let primary = accessibility_settings_url(crate::os::OperatingSystemVersion::get().major());
    let secondary = if primary == ACCESSIBILITY_SETTINGS_URL {
        ACCESSIBILITY_SETTINGS_URL_LEGACY
    } else {
        ACCESSIBILITY_SETTINGS_URL
    };
    open_settings_url(primary) || open_settings_url(secondary)
}

/// Ventura (13) introduced the PrivacySecurity Settings pane. On Monterey the
/// same `x-apple.systempreferences:` scheme still opens, so a successful
/// `openURL` on the new id is not proof the Accessibility list appeared.
pub(crate) fn accessibility_settings_url(major: isize) -> &'static str {
    if major >= 13 {
        ACCESSIBILITY_SETTINGS_URL
    } else {
        ACCESSIBILITY_SETTINGS_URL_LEGACY
    }
}

fn open_settings_url(url: &str) -> bool {
    let string = NSString::from_str(url);
    let Some(nsurl) = (unsafe { NSURL::initWithString(NSURL::alloc(), &string) }) else {
        return false;
    };
    let workspace = unsafe { NSWorkspace::sharedWorkspace() };
    unsafe { workspace.openURL(&nsurl) }
}

/// Raises the system TCC sheet. Product flows must not call this — they use
/// [`begin_accessibility_guide`] from the settings Grant button instead.
pub fn prompt_for_accessibility() -> bool {
    unsafe {
        let prompt_key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let prompt_value = CFBoolean::wrap_under_get_rule(kCFBooleanTrue);
        let options = CFDictionary::from_CFType_pairs(&[(prompt_key.as_CFType(), prompt_value.as_CFType())]);

        AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
    }
}

pub fn accessibility_is_enabled() -> bool {
    unsafe { AXIsProcessTrusted() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monterey_opens_the_legacy_accessibility_pane() {
        assert_eq!(accessibility_settings_url(12), ACCESSIBILITY_SETTINGS_URL_LEGACY);
        assert_eq!(accessibility_settings_url(13), ACCESSIBILITY_SETTINGS_URL);
        assert_eq!(accessibility_settings_url(26), ACCESSIBILITY_SETTINGS_URL);
    }
}
