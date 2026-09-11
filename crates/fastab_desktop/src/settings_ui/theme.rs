//! Theme data shared by the settings chrome and the completion-theme picker.
//!
//! The named palettes below are the same palettes shipped in `themes/*.json`.
//! The completion popup can use those files directly, while the settings
//! window needs a few additional surfaces (sidebar, cards and controls).  We
//! derive those surfaces from the popup palette here so selecting a named
//! theme keeps the two parts of the product visually related.

use fastab_gpui::system_appearance_is_dark;

const BUILTIN_LIGHT: Chrome = Chrome {
    bg: 0xf7f8fa,
    sidebar: 0xeff1f5,
    sidebar_border: 0xe0e4eb,
    text: 0x1d1d1f,
    muted: 0x6e6e73,
    card: 0xffffff,
    separator: 0xe5e5ea,
    accent: 0x007aff,
    accent_text: 0xffffff,
    track_off: 0xd1d1d6,
    selection: 0xe1edfc,
};

const BUILTIN_DARK: Chrome = Chrome {
    bg: 0x1c1c1e,
    sidebar: 0x161618,
    sidebar_border: 0x2c2c2e,
    text: 0xf5f5f7,
    muted: 0x8e8e93,
    card: 0x2c2c2e,
    separator: 0x3a3a3c,
    accent: 0x0a84ff,
    accent_text: 0xffffff,
    track_off: 0x48484a,
    selection: 0x19364f,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ThemeAppearance {
    System,
    Light,
    Dark,
}

impl ThemeAppearance {
    fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Light => false,
            Self::Dark => true,
        }
    }
}

pub(super) struct ThemeSwatch {
    pub(super) id: &'static str,
    pub(super) label_en: &'static str,
    pub(super) label_zh: &'static str,
    pub(super) appearance: ThemeAppearance,
}

/// The settings chrome offers the same twelve choices as the completion
/// popup: three built-ins plus nine bundled named palettes.
pub(super) const THEMES: &[ThemeSwatch] = &[
    ThemeSwatch {
        id: "system",
        label_en: "System",
        label_zh: "跟随系统",
        appearance: ThemeAppearance::System,
    },
    ThemeSwatch {
        id: "light",
        label_en: "Light",
        label_zh: "浅色",
        appearance: ThemeAppearance::Light,
    },
    ThemeSwatch {
        id: "github-light",
        label_en: "GitHub Light",
        label_zh: "GitHub Light",
        appearance: ThemeAppearance::Light,
    },
    ThemeSwatch {
        id: "claude-light",
        label_en: "Claude Light",
        label_zh: "Claude Light",
        appearance: ThemeAppearance::Light,
    },
    ThemeSwatch {
        id: "catppuccin-latte",
        label_en: "Catppuccin Latte",
        label_zh: "Catppuccin Latte",
        appearance: ThemeAppearance::Light,
    },
    ThemeSwatch {
        id: "dark",
        label_en: "Dark",
        label_zh: "深色",
        appearance: ThemeAppearance::Dark,
    },
    ThemeSwatch {
        id: "github-dark",
        label_en: "GitHub Dark",
        label_zh: "GitHub Dark",
        appearance: ThemeAppearance::Dark,
    },
    ThemeSwatch {
        id: "claude-dark",
        label_en: "Claude Dark",
        label_zh: "Claude Dark",
        appearance: ThemeAppearance::Dark,
    },
    ThemeSwatch {
        id: "nord",
        label_en: "Nord",
        label_zh: "Nord",
        appearance: ThemeAppearance::Dark,
    },
    ThemeSwatch {
        id: "gruvbox-dark",
        label_en: "Gruvbox Dark",
        label_zh: "Gruvbox Dark",
        appearance: ThemeAppearance::Dark,
    },
    ThemeSwatch {
        id: "one-dark",
        label_en: "One Dark",
        label_zh: "One Dark",
        appearance: ThemeAppearance::Dark,
    },
    ThemeSwatch {
        id: "tokyo-night",
        label_en: "Tokyo Night",
        label_zh: "Tokyo Night",
        appearance: ThemeAppearance::Dark,
    },
];

#[derive(Clone, Copy)]
pub(super) struct Chrome {
    pub(super) bg: u32,
    pub(super) sidebar: u32,
    pub(super) sidebar_border: u32,
    pub(super) text: u32,
    pub(super) muted: u32,
    pub(super) card: u32,
    pub(super) separator: u32,
    pub(super) accent: u32,
    pub(super) accent_text: u32,
    pub(super) track_off: u32,
    pub(super) selection: u32,
}

impl Chrome {
    /// Resolve the current dashboard theme without changing user settings.
    pub(super) fn current() -> Self {
        let theme = fastab_settings::settings::get_string_or("dashboard.theme", "system".into());
        Self::for_theme(&theme, system_appearance_is_dark())
    }

    /// Pure theme-name-to-chrome mapping. `system_dark` is supplied by the
    /// caller so the mapping remains deterministic and easy to test.
    fn for_theme(name: &str, system_dark: bool) -> Self {
        let name = name.trim().to_ascii_lowercase();
        match name.as_str() {
            "light" => BUILTIN_LIGHT,
            "dark" => BUILTIN_DARK,
            "system" => {
                if system_dark {
                    BUILTIN_DARK
                } else {
                    BUILTIN_LIGHT
                }
            },
            name => named_palette(name).map_or(if system_dark { BUILTIN_DARK } else { BUILTIN_LIGHT }, |palette| {
                palette.to_chrome()
            }),
        }
    }
}

#[derive(Clone, Copy)]
struct Palette {
    appearance: ThemeAppearance,
    background: u32,
    border: u32,
    text: u32,
    muted: u32,
    selected: u32,
    accent: u32,
}

impl Palette {
    fn to_chrome(self) -> Chrome {
        let dark = self.appearance.is_dark(false);
        let sidebar = if dark {
            mix(self.background, 0x000000, 0.18)
        } else {
            mix(self.background, 0x000000, 0.035)
        };
        let card = if dark {
            mix(self.background, 0xffffff, 0.08)
        } else {
            mix(self.background, 0xffffff, 0.55)
        };
        let selection = accessible_selection(self.background, self.selected, self.accent, self.text);

        Chrome {
            bg: self.background,
            sidebar,
            sidebar_border: self.border,
            text: self.text,
            muted: readable_muted(self.background, card, self.muted, self.text),
            card,
            separator: self.border,
            accent: self.accent,
            accent_text: if contrast_ratio(self.accent, 0xffffff) >= 4.5 {
                0xffffff
            } else {
                0x111111
            },
            track_off: if dark {
                mix(self.background, self.text, 0.28)
            } else {
                mix(self.background, self.text, 0.18)
            },
            selection,
        }
    }
}

/// Colors are copied from the corresponding `themes/*.json` files.  The
/// settings accent uses the palette's brighter accent swatch where the popup
/// selection is intentionally a neutral or low-contrast row highlight.
fn named_palette(name: &str) -> Option<Palette> {
    let appearance = THEMES.iter().find(|theme| theme.id == name)?.appearance;
    Some(match name {
        "github-light" => Palette {
            appearance,
            background: 0xffffff,
            border: 0xd0d7de,
            text: 0x24292f,
            muted: 0x57606a,
            selected: 0x0969da,
            accent: 0x0969da,
        },
        "claude-light" => Palette {
            appearance,
            background: 0xf3f1e9,
            border: 0xd9d5cc,
            text: 0x1a1917,
            muted: 0x6b665f,
            selected: 0xefe5db,
            accent: 0xa84b3a,
        },
        "catppuccin-latte" => Palette {
            appearance,
            background: 0xeff1f5,
            border: 0xccd0da,
            text: 0x4c4f69,
            muted: 0x6c6f85,
            selected: 0x1e66f5,
            accent: 0x1e66f5,
        },
        "github-dark" => Palette {
            appearance,
            background: 0x0d1117,
            border: 0x30363d,
            text: 0xc9d1d9,
            muted: 0x8b949e,
            selected: 0x1f6feb,
            accent: 0x1f6feb,
        },
        "claude-dark" => Palette {
            appearance,
            background: 0x262624,
            border: 0x363633,
            text: 0xf0eee6,
            muted: 0xa3a099,
            selected: 0x3d3d3a,
            accent: 0xcc785c,
        },
        "nord" => Palette {
            appearance,
            background: 0x2e3440,
            border: 0x3b4252,
            text: 0xd8dee9,
            muted: 0x4c566a,
            selected: 0x5e81ac,
            accent: 0x81a1c1,
        },
        "gruvbox-dark" => Palette {
            appearance,
            background: 0x282828,
            border: 0x3c3836,
            text: 0xebdbb2,
            muted: 0x928374,
            selected: 0x458588,
            accent: 0xd79921,
        },
        "one-dark" => Palette {
            appearance,
            background: 0x282c34,
            border: 0x3e4452,
            text: 0xabb2bf,
            muted: 0x5c6370,
            selected: 0x528bff,
            accent: 0x61afef,
        },
        "tokyo-night" => Palette {
            appearance,
            background: 0x1a1b26,
            border: 0x292e42,
            text: 0xa9b1d6,
            muted: 0x565f89,
            selected: 0x364a82,
            accent: 0x7aa2f7,
        },
        _ => return None,
    })
}

/// Keep selected chips and the active sidebar item readable when a source
/// theme's popup selection is too close to its UI accent.
fn accessible_selection(background: u32, selected: u32, accent: u32, text: u32) -> u32 {
    let readable = |surface| contrast_ratio(surface, accent) >= 3.0 && contrast_ratio(surface, text) >= 4.5;
    if readable(selected) {
        return selected;
    }
    // Use the strongest subtle tint that preserves both accent-colored
    // sidebar labels and normal text in the theme menu.
    for step in (1..=20).rev() {
        let candidate = mix(background, accent, step as f32 / 100.0);
        if readable(candidate) {
            return candidate;
        }
    }
    background
}

fn readable_muted(background: u32, card: u32, muted: u32, text: u32) -> u32 {
    if contrast_ratio(background, muted) >= 3.0 && contrast_ratio(card, muted) >= 3.0 {
        return muted;
    }
    for step in 1..=10 {
        let candidate = mix(muted, text, step as f32 / 10.0);
        if contrast_ratio(background, candidate) >= 3.0 && contrast_ratio(card, candidate) >= 3.0 {
            return candidate;
        }
    }
    text
}

fn mix(from: u32, to: u32, amount: f32) -> u32 {
    let amount = amount.clamp(0.0, 1.0);
    let from = channels(from);
    let to = channels(to);
    pack(
        lerp(from[0], to[0], amount),
        lerp(from[1], to[1], amount),
        lerp(from[2], to[2], amount),
    )
}

fn channels(color: u32) -> [u8; 3] {
    [
        ((color >> 16) & 0xff) as u8,
        ((color >> 8) & 0xff) as u8,
        (color & 0xff) as u8,
    ]
}

fn lerp(from: u8, to: u8, amount: f32) -> u8 {
    (f32::from(from) + (f32::from(to) - f32::from(from)) * amount).round() as u8
}

fn pack(red: u8, green: u8, blue: u8) -> u32 {
    u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue)
}

fn contrast_ratio(first: u32, second: u32) -> f32 {
    let first = relative_luminance(first);
    let second = relative_luminance(second);
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

fn relative_luminance(color: u32) -> f32 {
    let [red, green, blue] = channels(color);
    let linear = |channel: u8| {
        let value = f32::from(channel) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_twelve_unique_theme_ids() {
        let mut ids: Vec<_> = THEMES.iter().map(|theme| theme.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(THEMES.len(), 12);
        assert_eq!(ids.len(), THEMES.len());
    }

    #[test]
    fn named_themes_are_real_independent_palettes() {
        let chrome: Vec<_> = THEMES
            .iter()
            .filter_map(|theme| named_palette(theme.id).map(Palette::to_chrome))
            .collect();
        assert_eq!(chrome.len(), 9);
        assert!(chrome.iter().any(|theme| theme.bg == 0xffffff));
        assert!(chrome.iter().any(|theme| theme.bg == 0x0d1117));
        assert!(chrome.windows(2).all(|pair| pair[0].bg != pair[1].bg));
    }

    #[test]
    fn named_palettes_match_the_bundled_theme_files() {
        let themes_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes");
        for swatch in THEMES.iter().filter(|swatch| named_palette(swatch.id).is_some()) {
            let palette = named_palette(swatch.id).expect("named palette");
            let path = themes_dir.join(format!("{}.json", swatch.id));
            let text = std::fs::read_to_string(&path).expect("bundled theme file");
            let source = fastab_gpui::theme_from_json(&text).expect("parse bundled theme");

            // These fields are copied directly from the popup palette. The
            // settings-only surfaces are derived from them in `to_chrome`.
            assert_eq!(palette.background, source.background, "{} background", swatch.id);
            assert_eq!(palette.border, source.border, "{} border", swatch.id);
            assert_eq!(palette.text, source.text, "{} text", swatch.id);
            assert_eq!(palette.muted, source.muted, "{} muted", swatch.id);
            assert_eq!(palette.selected, source.selected, "{} selection", swatch.id);
        }
    }

    #[test]
    fn light_and_dark_catalog_entries_have_matching_appearance() {
        for theme in THEMES {
            match theme.appearance {
                ThemeAppearance::Light => {
                    let chrome = Chrome::for_theme(theme.id, false);
                    assert!(relative_luminance(chrome.bg) > 0.5, "{}", theme.id);
                },
                ThemeAppearance::Dark => {
                    let chrome = Chrome::for_theme(theme.id, true);
                    assert!(relative_luminance(chrome.bg) < 0.5, "{}", theme.id);
                },
                ThemeAppearance::System => {
                    assert_eq!(Chrome::for_theme(theme.id, false).bg, BUILTIN_LIGHT.bg);
                    assert_eq!(Chrome::for_theme(theme.id, true).bg, BUILTIN_DARK.bg);
                },
            }
        }
    }

    #[test]
    fn interactive_accent_has_readable_selected_surface() {
        for theme in THEMES {
            let chrome = Chrome::for_theme(theme.id, false);
            assert!(
                contrast_ratio(chrome.selection, chrome.accent) >= 3.0,
                "{} selection {:06x} / accent {:06x}",
                theme.id,
                chrome.selection,
                chrome.accent
            );
        }
    }

    #[test]
    fn chrome_surfaces_keep_text_and_layers_distinct() {
        for system_dark in [false, true] {
            for theme in THEMES {
                let chrome = Chrome::for_theme(theme.id, system_dark);
                assert!(
                    contrast_ratio(chrome.bg, chrome.text) >= 4.5,
                    "{} text {:06x} / background {:06x}",
                    theme.id,
                    chrome.text,
                    chrome.bg
                );
                assert!(
                    contrast_ratio(chrome.bg, chrome.muted) >= 3.0,
                    "{} muted {:06x} / background {:06x}",
                    theme.id,
                    chrome.muted,
                    chrome.bg
                );
                assert!(
                    contrast_ratio(chrome.selection, chrome.accent) >= 3.0,
                    "{} selection {:06x} / accent {:06x}",
                    theme.id,
                    chrome.selection,
                    chrome.accent
                );
                assert!(
                    contrast_ratio(chrome.card, chrome.muted) >= 3.0,
                    "{} muted/card",
                    theme.id
                );
                assert!(
                    contrast_ratio(chrome.selection, chrome.text) >= 4.5,
                    "{} menu text/selection",
                    theme.id
                );
                if named_palette(theme.id).is_some() {
                    assert!(
                        contrast_ratio(chrome.accent, chrome.accent_text) >= 4.5,
                        "{} filled button text",
                        theme.id
                    );
                }
                assert!(
                    chrome.bg != chrome.card || contrast_ratio(chrome.separator, chrome.card) >= 1.2,
                    "{} card needs a distinct fill or border",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn system_and_unknown_names_follow_the_requested_system_appearance() {
        assert_eq!(Chrome::for_theme("system", false).bg, BUILTIN_LIGHT.bg);
        assert_eq!(Chrome::for_theme("system", true).bg, BUILTIN_DARK.bg);
        assert_eq!(Chrome::for_theme("future-theme", false).bg, BUILTIN_LIGHT.bg);
        assert_eq!(Chrome::for_theme("future-theme", true).bg, BUILTIN_DARK.bg);
    }
}
