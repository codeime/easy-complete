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
/// Bundle-ID table and caret wire format are OS-agnostic. Compiled on every
/// OS so Linux CI pins IMK terminals and the prost-equivalent frame.
#[allow(dead_code)]
mod terminals;
#[allow(dead_code)]
mod wire;

#[cfg(not(target_os = "macos"))]
use std::process::ExitCode;

#[cfg(target_os = "macos")]
pub use macos::main;

#[cfg(not(target_os = "macos"))]
fn main() -> ExitCode {
    println!("Easy Complete input method is only supported on macOS");
    ExitCode::FAILURE
}

#[cfg(test)]
mod macos_pins;
