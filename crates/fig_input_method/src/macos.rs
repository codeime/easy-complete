use std::os::raw::c_void;

use core_foundation::array::CFArrayRef;
use core_foundation::base::{CFRelease, CFTypeRef, OSStatus, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation::url::CFURL;
use objc2::rc::autoreleasepool;
use objc2::runtime::Bool;
use objc2::{ClassType, msg_send};
use objc2_app_kit::NSApp;
use objc2_foundation::{MainThreadMarker, NSBundle, NSObject, ns_string};

use crate::imk;

const CONNECTION_NAME: &str = env!("InputMethodConnectionName");

type TISInputSourceRef = *const c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISRegisterInputSource(location: *const core_foundation::url::__CFURL) -> OSStatus;

    static kTISPropertyInputSourceID: CFStringRef;
    static kTISPropertyInputSourceIsEnabled: CFStringRef;

    fn TISCreateInputSourceList(properties: CFDictionaryRef, include_all_installed: bool) -> CFArrayRef;
    fn TISGetInputSourceProperty(input_source: TISInputSourceRef, key: CFStringRef) -> CFTypeRef;
    fn TISEnableInputSource(input_source: TISInputSourceRef) -> OSStatus;
    fn TISSelectInputSource(input_source: TISInputSourceRef) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: isize) -> *const c_void;
}

fn with_self_input_source(input_source_id: &str, work: impl FnOnce(TISInputSourceRef)) -> bool {
    let id_value = CFString::new(input_source_id);
    let key = unsafe { CFString::wrap_under_get_rule(kTISPropertyInputSourceID) };
    let dict = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), id_value.as_CFType())]);

    // `include_all_installed = true` so we find the source even while it is still disabled.
    let list = unsafe { TISCreateInputSourceList(dict.as_concrete_TypeRef(), true) };
    if list.is_null() {
        log_info!("with_self_input_source: TISCreateInputSourceList returned null (no NSApplication context?)");
        return false;
    }

    let count = unsafe { CFArrayGetCount(list) };
    if count <= 0 {
        log_info!("with_self_input_source: no input source found for {input_source_id}");
        unsafe { CFRelease(list.cast()) };
        return false;
    }

    // Borrowed pointer into `list`; valid until we release the list below.
    let src: TISInputSourceRef = unsafe { CFArrayGetValueAtIndex(list, 0) };
    work(src);
    unsafe { CFRelease(list.cast()) };
    true
}

fn source_is_enabled(src: TISInputSourceRef) -> bool {
    unsafe {
        let value = TISGetInputSourceProperty(src, kTISPropertyInputSourceIsEnabled);
        !value.is_null() && CFBoolean::wrap_under_get_rule(value.cast()).into()
    }
}

/// Enable (and select) our own input source via the TIS API.
///
/// The CLI installer (`ec integrations install input-method`) cannot do this: TIS
/// APIs need an `NSApplication` run loop, which the CLI lacks, so `TISEnableInputSource`
/// silently fails there. The IME process *does* have that context, so it enables itself
/// on startup. Without this, terminals that depend on the IME for cursor tracking
/// (Ghostty, Kitty, WezTerm, Zed, Alacritty) get the autocomplete window stuck at a
/// default position instead of following the caret.
///
/// Returns whether a freshly looked-up source reports enabled. Callers must not
/// trust the property on a source they just mutated: TIS caches it on the
/// pointer, and `TISEnableInputSource` returning 0 does not mean it stuck.
fn enable_self_in_tis(input_source_id: &str) -> bool {
    with_self_input_source(input_source_id, |src| {
        if source_is_enabled(src) {
            log_info!("enable_self_in_tis: input source already enabled");
        } else {
            let status = unsafe { TISEnableInputSource(src) };
            log_info!("enable_self_in_tis: TISEnableInputSource status = {status}");
        }

        // Selecting a `palette` (non-keyboard) input method returns paramErr (-50); that is
        // expected and harmless — enabling alone makes it active alongside the keyboard.
        let select_status = unsafe { TISSelectInputSource(src) };
        log_info!("enable_self_in_tis: TISSelectInputSource status = {select_status}");
    });
    source_is_enabled_fresh(input_source_id)
}

/// `TISEnableInputSource` does not reliably write `AppleEnabledInputSources`.
/// After bounce we were selected-only; new Otty windows then got no IMK
/// connection. Add ourselves to both palette lists. Never remove other sources
/// except a stale vendor-prefix rename of this same palette.
///
/// In-process, not through `python3`: TIS launches this bundle with a bare
/// `PATH`, where `python3` is the Command Line Tools stub and fails outright on
/// a machine that never installed them. That failure costs the caret.
fn persist_palette_in_hitoolbox(bundle_id: &str) {
    if ec_hitoolbox::is_palette_enabled(bundle_id) {
        return;
    }

    if ec_hitoolbox::ensure_palette_enabled(bundle_id) {
        log_info!("persist_palette_in_hitoolbox: wrote {bundle_id} into enabled+selected");
    } else {
        log_error!("persist_palette_in_hitoolbox: could not write the HIToolbox palette lists for {bundle_id}");
    }
}

fn source_is_enabled_fresh(input_source_id: &str) -> bool {
    let mut enabled = false;
    with_self_input_source(input_source_id, |src| {
        enabled = source_is_enabled(src);
    });
    enabled
}

fn register_self_with_tis() {
    // Get the bundle path and register with TIS so macOS routes IMK connections to us
    let bundle = objc2_foundation::NSBundle::mainBundle();
    let bundle_path = unsafe { bundle.bundlePath() };
    let path_str = bundle_path.to_string();
    if let Some(url) = CFURL::from_path(&path_str, true) {
        let result = unsafe { TISRegisterInputSource(url.as_concrete_TypeRef()) };
        log_info!("TISRegisterInputSource result: {result}");
    }
}

pub fn main() {
    // Default is ERROR, same as the desktop app. The previous `trace` filter
    // plus an INFO `respondsToSelector` probe wrote on every IMK query and
    // kept a multi-thread tokio runtime alive for a process that only needs
    // AppKit. `Q_LOG_LEVEL=debug` still raises this when diagnosing caret
    // delivery.
    crate::logging::init();

    log_info!("Registering imk controller");
    imk::register_controller();
    log_info!("Registered imk controller");

    let Some(mtm) = MainThreadMarker::new() else {
        log_error!("IME must start on the AppKit main thread");
        return;
    };

    autoreleasepool(|_pool| {
        let app = NSApp(mtm);

        let k_connection_name = ns_string!(CONNECTION_NAME);
        let nib_name = ns_string!("MainMenu");

        let bundle = NSBundle::mainBundle();
        let identifier = unsafe { bundle.bundleIdentifier() };

        register_self_with_tis();

        log_info!("Attempting connection...");
        imk::connect_imkserver(k_connection_name, identifier.as_deref());
        log_info!("Connected!");

        // Enable ourselves. Do not disable first: a disable→enable cycle, even
        // across later runloop turns, leaves a palette source disabled and
        // strips the caret from every Otty / Ghostty / Kitty window.
        // TISEnableInputSource can report success and still omit us from
        // AppleEnabledInputSources; persist the plist ourselves so a new
        // Otty window can attach.
        if let Some(id) = identifier.as_deref() {
            let id = id.to_string();
            if !enable_self_in_tis(&id) {
                log_error!("enable_self_in_tis: {id} is still disabled; IME-only terminals will have no caret");
            }
            persist_palette_in_hitoolbox(&id);
        } else {
            log_info!("Could not determine bundle identifier; skipping TIS self-enable");
        }

        let app_id: &NSObject = app.as_ref();
        let loaded_nib: Bool = unsafe { msg_send![NSBundle::class(), loadNibNamed:nib_name owner:app_id] };
        log_info!("RUNNING {loaded_nib:?}!");

        unsafe { app.run() };
    });
}
