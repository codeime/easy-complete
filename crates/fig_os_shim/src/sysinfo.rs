use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use crate::Shim;

#[derive(Debug, Clone, Default)]
pub struct SysInfo(inner::Inner);

mod inner {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Default)]
    pub enum Inner {
        #[default]
        Real,
        Fake(Arc<Mutex<Fake>>),
    }

    #[derive(Debug, Clone, Default)]
    pub struct Fake {
        pub process_names: HashSet<String>,
    }
}

fn fake_lock(fake: &Mutex<inner::Fake>) -> std::sync::MutexGuard<'_, inner::Fake> {
    fake.lock().unwrap_or_else(|err| err.into_inner())
}

impl SysInfo {
    pub fn new_fake() -> Self {
        Self(inner::Inner::Fake(Arc::new(Mutex::new(inner::Fake::default()))))
    }

    /// Returns whether the process containing `name` is running.
    pub fn is_process_running(&self, name: &str) -> bool {
        use inner::Inner;
        match &self.0 {
            Inner::Real => {
                let system = sysinfo::System::new_all();

                system.processes_by_name(&OsString::from(name)).next().is_some()
            },
            Inner::Fake(fake) => fake_lock(fake).process_names.contains(name),
        }
    }

    pub fn add_running_processes(&self, process_names: &[&str]) {
        use inner::Inner;
        match &self.0 {
            Inner::Real => {
                // Test helper: a live process list cannot be injected.
            },
            Inner::Fake(fake) => {
                let curr_names = &mut fake_lock(fake).process_names;
                for name in process_names {
                    curr_names.insert((*name).to_string());
                }
            },
        }
    }
}

impl Shim for SysInfo {
    fn is_real(&self) -> bool {
        matches!(self.0, inner::Inner::Real)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_sysinfo_lock_recovers_from_poison() {
        let info = SysInfo::new_fake();
        info.add_running_processes(&["ecterm"]);
        let inner::Inner::Fake(fake) = &info.0 else {
            panic!("expected fake sysinfo");
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = fake.lock().unwrap();
            panic!("poison");
        }));
        assert!(info.is_process_running("ecterm"));
        info.add_running_processes(&["easy-complete"]);
        assert!(info.is_process_running("easy-complete"));
    }

    #[test]
    fn real_add_running_processes_does_not_panic() {
        SysInfo::default().add_running_processes(&["ecterm"]);
    }

    #[test]
    fn fake_sysinfo_does_not_unwrap_the_lock() {
        let production = include_str!("sysinfo.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            !production.contains(".lock().unwrap()")
                && !production.contains("panic!(\"unimplemented\")")
                && production.contains("fake_lock")
                && production.contains("into_inner"),
            "a poisoned Fake sysinfo mutex must recover; Real add_running_processes is a no-op"
        );
    }
}
