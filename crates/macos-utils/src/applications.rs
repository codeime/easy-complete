use objc2_app_kit::{NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSString, NSURL};

#[derive(Debug)]
pub struct MacOSApplication {
    pub name: Option<String>,
    pub bundle_identifier: Option<String>,
    pub bundle_path: Option<String>,
    pub process_identifier: libc::pid_t,
}

pub fn running_applications() -> Vec<MacOSApplication> {
    unsafe {
        let workspace = NSWorkspace::sharedWorkspace();
        let apps = workspace.runningApplications();
        apps.iter()
            .map(|app| {
                let name = app.localizedName().map(|s| s.to_string());
                let bundle_identifier = app.bundleIdentifier().map(|s| s.to_string());
                let bundle_path = app.bundleURL().and_then(|url| url.path()).map(|s| s.to_string());
                let process_identifier = app.processIdentifier();

                MacOSApplication {
                    name,
                    bundle_identifier,
                    bundle_path,
                    process_identifier,
                }
            })
            .collect()
    }
}

/// PIDs of the running applications with this bundle identifier.
///
/// Asks AppKit for the match instead of enumerating every running application
/// and allocating its name, bundle id and path the way [`running_applications`]
/// does, which makes it cheap enough to poll in a wait loop.
pub fn running_application_pids(bundle_identifier: &str) -> Vec<libc::pid_t> {
    let identifier = NSString::from_str(bundle_identifier);
    let apps = unsafe { NSRunningApplication::runningApplicationsWithBundleIdentifier(&identifier) };
    apps.iter().map(|app| unsafe { app.processIdentifier() }).collect()
}

pub fn launch_application(bundle_path: &str) {
    let bundle_nsstring = NSString::from_str(bundle_path);
    let bundle_nsurl = unsafe { NSURL::fileURLWithPath_isDirectory(&bundle_nsstring, true) };

    let workspace = unsafe { NSWorkspace::sharedWorkspace() };
    unsafe { workspace.openURL(&bundle_nsurl) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_running_applications() {
        let applications = running_applications();
        println!("{applications:#?}");
    }

    /// [`running_application_pids`] decides whether callers consider a helper to
    /// be up, so it has to see everything the full enumeration sees.
    #[test]
    fn pid_query_agrees_with_the_full_enumeration() {
        for app in running_applications() {
            let Some(bundle_id) = app.bundle_identifier.as_deref() else {
                continue;
            };
            if running_application_pids(bundle_id).contains(&app.process_identifier) {
                continue;
            }

            // A process that exited between the two calls is not a mismatch.
            let still_running = running_applications()
                .iter()
                .any(|other| other.process_identifier == app.process_identifier);
            assert!(
                !still_running,
                "{bundle_id} (pid {}) is running but the bundle id query missed it",
                app.process_identifier
            );
        }
    }
}
