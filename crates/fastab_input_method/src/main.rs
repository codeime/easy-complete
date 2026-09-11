// `logging` must come first: it defines the `log_*` macros the other modules use.
#[cfg(target_os = "macos")]
#[macro_use]
mod logging;

#[cfg(target_os = "macos")]
mod imk;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod paths;
#[cfg(target_os = "macos")]
mod terminals;
#[cfg(target_os = "macos")]
mod wire;

#[cfg(not(target_os = "macos"))]
use std::process::ExitCode;

#[cfg(target_os = "macos")]
pub use macos::main;

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    println!("Fig input method is only supported on macOS");
    ExitCode::FAILURE
}
