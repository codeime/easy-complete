//! Which bundle IDs report their caret through IMK.
//!
//! Mirrors `Terminal::from_bundle_id` composed with
//! `Terminal::supports_macos_input_method`, for the same reason as [`crate::paths`]:
//! `fig_util` is too heavy to link here. [`tests::matches_fig_util`] pins the two
//! together over the full bundle-ID table. Compiled on every OS so Linux CI
//! runs that pin; `imk.rs` is still `cfg(macos)`.

/// Terminals that draw their own cursor and expose it only through the input
/// method: the desktop cannot read their caret over Accessibility.
pub fn supports_input_method(bundle_id: &str) -> bool {
    matches!(
        bundle_id,
        "io.alacritty"
            | "org.alacritty"
            | "net.kovidgoyal.kitty"
            | "com.panic.Nova"
            | "com.github.wez.wezterm"
            | "dev.zed.Zed"
            | "com.raphaelamorim.rio"
            | "com.mitchellh.ghostty"
            | "io.appmakes.otty" // Every JetBrains IDE, plus Android Studio under `com.google.`, maps to
                                 // `Terminal::IntelliJ`, which supports the input method.
    ) || bundle_id.starts_with("com.jetbrains.")
        || bundle_id.starts_with("com.google.")
}

/// Alacritty needs a marked-text round trip on activation before winit turns its
/// IME on, so it is the one terminal the controller special-cases.
pub fn is_alacritty(bundle_id: &str) -> bool {
    matches!(bundle_id, "io.alacritty" | "org.alacritty")
}

#[cfg(test)]
mod tests {
    use fig_util::Terminal;

    use super::*;

    /// Every bundle ID `Terminal::from_bundle_id` knows, plus near-misses and the
    /// prefix-matched families.
    const CORPUS: &[&str] = &[
        "com.googlecode.iterm2",
        "com.apple.Terminal",
        "co.zeit.hyper",
        "io.alacritty",
        "org.alacritty",
        "net.kovidgoyal.kitty",
        "com.microsoft.VSCode",
        "com.microsoft.VSCodeInsiders",
        "com.vscodium",
        "com.visualstudio.code.oss",
        "org.tabby",
        "com.panic.Nova",
        "com.github.wez.wezterm",
        "dev.zed.Zed",
        "com.todesktop.230313mzl4w4u92",
        "com.todesktop.23052492jqa5xjo",
        "com.raphaelamorim.rio",
        "com.exafunction.windsurf",
        "com.exafunction.windsurf-next",
        "com.mitchellh.ghostty",
        "co.posit.positron",
        "com.trae.app",
        "io.appmakes.otty",
        "com.openai.codex",
        "com.jetbrains.intellij",
        "com.jetbrains.pycharm",
        "com.google.android.studio",
        "com.jetbrains",
        "com.googlecode",
        "org.alacritty.extra",
        "",
        "dev.emmmm.easy-complete",
    ];

    #[test]
    fn matches_fig_util() {
        for bundle_id in CORPUS {
            let expected =
                Terminal::from_bundle_id(bundle_id).is_some_and(|terminal| terminal.supports_macos_input_method());
            assert_eq!(
                supports_input_method(bundle_id),
                expected,
                "disagreed with fig_util for {bundle_id:?}"
            );
        }
    }

    #[test]
    fn alacritty_detection_matches_fig_util() {
        for bundle_id in CORPUS {
            let expected = matches!(Terminal::from_bundle_id(bundle_id), Some(Terminal::Alacritty));
            assert_eq!(
                is_alacritty(bundle_id),
                expected,
                "disagreed with fig_util for {bundle_id:?}"
            );
        }
    }
}
