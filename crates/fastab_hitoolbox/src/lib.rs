//! The two `com.apple.HIToolbox` palette lists that decide whether a terminal
//! window gets an IMK connection.
//!
//! `TISEnableInputSource` can return success and still leave the source out of
//! `AppleEnabledInputSources`, which is the list a new Otty / Ghostty / Kitty
//! window is matched against. Both the IME (from its own `NSApplication`) and
//! `ec integrations install input-method` (from a CLI process with no run loop)
//! have to repair that, so the write lives here rather than in either of them:
//! an install runs both at once, and two whole-domain `defaults export`/`import`
//! passes would race and drop each other's entry. These are key-scoped writes.
//!
//! This is also why the write is in-process instead of shelling out to
//! `python3`: the IME is launched by TIS with a bare `PATH`, and `python3` there
//! is the Command Line Tools stub, which is missing on a machine that never
//! installed them. A failed palette write means no caret and no overlay.

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{ensure_palette_enabled, is_palette_enabled};

/// HIToolbox is macOS-only; the rest of the workspace still compiles elsewhere.
#[cfg(not(target_os = "macos"))]
pub fn is_palette_enabled(_bundle_id: &str) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn ensure_palette_enabled(_bundle_id: &str) -> bool {
    false
}
