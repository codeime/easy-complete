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
#[cfg(unix)]
fn bash_dotfiles_install_and_uninstall_in_temp_home() -> Result<()> {
    let tmp = tempdir()?;
    let home = tmp.path();
    let data = home.join(".local/share");

    cli()
        .args(["integrations", "install", "--silent", "dotfiles", "bash"])
        .env("HOME", home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .assert()
        .success();

    let bashrc = fs::read_to_string(home.join(".bashrc")).unwrap_or_default();
    assert!(
        bashrc.contains(&format!("{PRODUCT_NAME} pre block")) && bashrc.contains(&format!("{PRODUCT_NAME} post block")),
        ".bashrc should contain pre/post hooks:\n{bashrc}"
    );

    cli()
        .args(["integrations", "uninstall", "--silent", "dotfiles", "bash"])
        .env("HOME", home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .assert()
        .success();

    let bashrc = fs::read_to_string(home.join(".bashrc")).unwrap_or_default();
    assert!(
        !bashrc.contains(&format!("{PRODUCT_NAME} pre block"))
            && !bashrc.contains(&format!("{PRODUCT_NAME} post block")),
        ".bashrc should drop hooks on uninstall:\n{bashrc}"
    );

    Ok(())
}

#[test]
#[cfg(unix)]
fn fish_confd_install_and_uninstall_in_temp_home() -> Result<()> {
    let tmp = tempdir()?;
    let home = tmp.path();
    let data = home.join(".local/share");
    let config = home.join(".config");

    cli()
        .args(["integrations", "install", "--silent", "dotfiles", "fish"])
        .env("HOME", home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .assert()
        .success();

    let pre = fs::read_to_string(config.join("fish/conf.d/00_fig_pre.fish")).unwrap_or_default();
    let post = fs::read_to_string(config.join("fish/conf.d/99_fig_post.fish")).unwrap_or_default();
    assert!(
        pre.contains("init fish pre") && post.contains("init fish post"),
        "fish conf.d should source ec init pre/post:\npre:\n{pre}\npost:\n{post}"
    );

    cli()
        .args(["integrations", "uninstall", "--silent", "dotfiles", "fish"])
        .env("HOME", home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .assert()
        .success();

    assert!(
        !config.join("fish/conf.d/00_fig_pre.fish").exists() && !config.join("fish/conf.d/99_fig_post.fish").exists(),
        "fish conf.d hooks should be removed on uninstall"
    );

    Ok(())
}

#[test]
#[cfg(not(target_os = "macos"))]
fn input_method_is_macos_only() -> Result<()> {
    cli()
        .args(["integrations", "install", "--silent", "input-method"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("only supported on macOS"));
    Ok(())
}
