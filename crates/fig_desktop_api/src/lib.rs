//! What is left of the WebView API bridge. The overlay and settings window are
//! native now, so only the install request survives: the desktop app and its
//! local IPC still speak to shell/SSH/autostart integrations through it.
pub mod requests;
