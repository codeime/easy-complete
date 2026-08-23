//! Leftover install request from the WebView API bridge.
//!
//! Overlay and settings are GPUI now, so only `requests::install` survives:
//! fig_desktop local IPC and the settings permission-repair path still call it.
//! Not a public crate. Not in workspace `default-members` (`cargo test` without
//! `--workspace` / `-p` does not build this). macOS dist still links it through
//! fig_desktop — do not delete it.
pub mod requests;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_not_a_default_workspace_member() {
        let workspace = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"));
        assert!(
            workspace
                .lines()
                .any(|line| line.trim() == "default-members = [\"crates/ec_cli\"]"),
            "fig_desktop_api must stay out of default-members"
        );
    }
}
