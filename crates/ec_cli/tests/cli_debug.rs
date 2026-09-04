mod common;

use common::*;

#[test]
fn debug_root() -> Result<()> {
    cli().arg("debug").assert().code(predicate::eq(2));
    Ok(())
}

#[test]
fn debug_drive_overlay_help() -> Result<()> {
    cli()
        .args(["debug", "drive-overlay", "--help"])
        .assert()
        .success();
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
fn debug_verify_codesign() -> Result<()> {
    cli()
        .args(["debug", "verify-codesign"])
        .assert()
        .code(predicate::in_iter([0, 1]));
    Ok(())
}
