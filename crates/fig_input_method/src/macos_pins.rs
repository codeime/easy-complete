//! Linux-runnable pins for the macOS IME startup policy.
//!
//! `macos.rs` / `imk.rs` stay `cfg(macos)` and talk to AppKit. `include_str`
//! of those files still compiles here, so rust-linux can fail a change that
//! brings `TISDisableInputSource` or a `python3` palette write back.

fn macos_production() -> String {
    include_str!("macos.rs")
        .split("#[cfg(test)]")
        .next()
        .expect("production source")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn imk_production() -> String {
    include_str!("imk.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn startup_never_disables_the_input_source() {
    let prod = macos_production();
    assert!(
        !prod.contains("TISDisableInputSource"),
        "disable after enable is what took Otty down on install"
    );
    assert!(!prod.contains("schedule_reconnect"));
    assert!(
        prod.contains("ec_hitoolbox::ensure_palette_enabled"),
        "TIS enable alone does not persist the palette"
    );
}

/// TIS launches this bundle with a bare `PATH`. A palette write that shells
/// out is a write that fails on a machine without Command Line Tools, and a
/// failed palette write means no caret in Otty / Ghostty / Kitty.
#[test]
fn the_palette_write_spawns_nothing() {
    assert!(!macos_production().contains("Command::new"));
}

#[test]
fn imk_caret_uses_the_shared_usable_and_coalesce_policy() {
    let prod = imk_production();
    assert!(
        prod.contains("caret_rect_is_usable"),
        "IMK must drop a zero-height / non-finite rect through the shared gate"
    );
    assert!(
        prod.contains("caret_should_replace"),
        "IMK must coalesce duplicate caret frames through the shared epsilon"
    );
    assert!(
        !prod.contains("fn is_valid_caret_rect") && !prod.contains("fn caret_rects_close"),
        "do not fork the IMK caret gates back into imk.rs"
    );
}
