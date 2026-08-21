//! Shell-hook install/uninstall in an isolated HOME.
//!
//! Linux v1 keeps the same `ec init` stand-down as macOS (no desktop → no
//! hooks). This test only covers writing and removing rc snippets; it does
//! not launch the desktop app or the input method.

mod common;

use std::fs;

use common::*;
use fig_util::PRODUCT_NAME;
use tempfile::tempdir;

#[test]
#[cfg(unix)]
fn zsh_dotfiles_install_and_uninstall_in_temp_home() -> Result<()> {
    let tmp = tempdir()?;
    let home = tmp.path();
    let data = home.join(".local/share");

    cli()
        .args(["integrations", "install", "--silent", "dotfiles", "zsh"])
        .env("HOME", home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("ZDOTDIR")
        .assert()
        .success();

    let zshrc = fs::read_to_string(home.join(".zshrc")).unwrap_or_default();
    let zprofile = fs::read_to_string(home.join(".zprofile")).unwrap_or_default();
    assert!(
        zshrc.contains(&format!("{PRODUCT_NAME} pre block")) && zshrc.contains(&format!("{PRODUCT_NAME} post block")),
        ".zshrc should contain pre/post hooks:\n{zshrc}"
    );
    assert!(
        zprofile.contains(&format!("{PRODUCT_NAME} pre block")),
        ".zprofile should contain the pre hook:\n{zprofile}"
    );

    cli()
        .args(["integrations", "uninstall", "--silent", "dotfiles", "zsh"])
        .env("HOME", home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("ZDOTDIR")
        .assert()
        .success();

    let zshrc = fs::read_to_string(home.join(".zshrc")).unwrap_or_default();
    let zprofile = fs::read_to_string(home.join(".zprofile")).unwrap_or_default();
    assert!(
        !zshrc.contains(&format!("{PRODUCT_NAME} pre block")) && !zshrc.contains(&format!("{PRODUCT_NAME} post block")),
        ".zshrc should drop hooks on uninstall:\n{zshrc}"
    );
    assert!(
        !zprofile.contains(&format!("{PRODUCT_NAME} pre block")),
        ".zprofile should drop hooks on uninstall:\n{zprofile}"
    );

    Ok(())
}

#[test]
#[cfg(not(target_os = "macos"))]
fn input_method_is_macos_only() -> Result<()> {
    cli()
        .args(["integrations", "install", "input-method", "--silent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("only supported on macOS"));
    Ok(())
}
