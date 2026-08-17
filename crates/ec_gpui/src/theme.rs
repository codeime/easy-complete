//! Autocomplete overlay themes. These reproduce the WebView overlay's
//! `src/fig/themes.ts`, last present in `packages/autocomplete-app` at v2.2.2.

use serde::Deserialize;

use crate::list::OverlayTheme;

#[derive(Debug, Deserialize)]
struct ThemeFile {
    theme: Option<ThemeColors>,
    #[serde(rename = "shade0")]
    shade0: Option<String>,
    #[serde(rename = "shade1")]
    shade1: Option<String>,
    #[serde(rename = "shade7")]
    shade7: Option<String>,
    #[serde(rename = "accent5")]
    accent5: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ThemeColors {
    #[serde(rename = "backgroundColor")]
    background_color: String,
    #[serde(rename = "textColor")]
    text_color: String,
    #[serde(rename = "matchBackgroundColor")]
    match_background_color: String,
    selection: SelectionColors,
    description: DescriptionColors,
}

#[derive(Debug, Deserialize)]
struct SelectionColors {
    #[serde(rename = "backgroundColor")]
    background_color: String,
    #[serde(rename = "textColor")]
    text_color: String,
    #[serde(rename = "matchBackgroundColor")]
    match_background_color: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DescriptionColors {
    #[serde(rename = "textColor")]
    text_color: String,
    #[serde(rename = "borderColor")]
    border_color: String,
}

/// Parse `#rgb`, `#rrggbb`, `#rrggbbaa`, or `rgb(r,g,b)` into `0xRRGGBB`.
pub fn parse_color(input: &str) -> Option<u32> {
    let input = input.trim();
    if let Some(hex) = input.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = input.to_ascii_lowercase();
    let rgb = lower.strip_prefix("rgb(")?.strip_suffix(')')?;
    let mut parts = rgb.split(',').map(|part| part.trim().parse::<u32>().ok());
    let r = parts.next()??;
    let g = parts.next()??;
    let b = parts.next()??;
    Some(((r & 0xff) << 16) | ((g & 0xff) << 8) | (b & 0xff))
}

fn parse_hex(hex: &str) -> Option<u32> {
    match hex.len() {
        3 => {
            let n = u32::from_str_radix(hex, 16).ok()?;
            let r = (n >> 8) & 0xf;
            let g = (n >> 4) & 0xf;
            let b = n & 0xf;
            Some((r << 20) | (r << 16) | (g << 12) | (g << 8) | (b << 4) | b)
        },
        6 => u32::from_str_radix(hex, 16).ok(),
        8 => u32::from_str_radix(&hex[..6], 16).ok(),
        _ => None,
    }
}

pub fn theme_from_json(text: &str) -> Option<OverlayTheme> {
    let file: ThemeFile = serde_json::from_str(text).ok()?;
    if let Some(theme) = file.theme {
        Some(OverlayTheme {
            background: parse_color(&theme.background_color)?,
            border: parse_color(&theme.description.border_color)?,
            text: parse_color(&theme.text_color)?,
            muted: parse_color(&theme.description.text_color)?,
            selected: parse_color(&theme.selection.background_color)?,
            selected_text: parse_color(&theme.selection.text_color)?,
            match_bg: parse_color(&theme.match_background_color)?,
            selected_match_bg: theme
                .selection
                .match_background_color
                .as_deref()
                .and_then(parse_color)
                .unwrap_or(0x6a8eda),
            accent: parse_color(&theme.selection.background_color)?,
        })
    } else {
        Some(OverlayTheme {
            background: parse_color(file.shade0.as_deref()?)?,
            border: parse_color(file.shade1.as_deref()?)?,
            text: parse_color(file.shade7.as_deref()?)?,
            muted: parse_color(file.shade7.as_deref()?)?,
            selected: parse_color(file.accent5.as_deref()?)?,
            selected_text: 0xfdfdfd,
            match_bg: parse_color(file.accent5.as_deref()?)?,
            selected_match_bg: 0x6a8eda,
            accent: parse_color(file.accent5.as_deref()?)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_and_rgb() {
        assert_eq!(parse_color("#0d1117"), Some(0x0d1117));
        assert_eq!(parse_color("#fff"), Some(0xffffff));
        assert_eq!(parse_color("rgb(48,48,48)"), Some(0x303030));
        assert_eq!(parse_color("rgb(30, 90, 199)"), Some(0x1e5ac7));
    }

    #[test]
    fn github_dark_json_maps_selection_and_description() {
        let theme = theme_from_json(
            r##"{
              "theme": {
                "backgroundColor": "#0d1117",
                "textColor": "#c9d1d9",
                "matchBackgroundColor": "#3d2200",
                "selection": {
                  "backgroundColor": "#1f6feb",
                  "textColor": "#ffffff",
                  "matchBackgroundColor": "#388bfd"
                },
                "description": {
                  "textColor": "#8b949e",
                  "borderColor": "#30363d"
                }
              }
            }"##,
        )
        .expect("theme");
        assert_eq!(theme.background, 0x0d1117);
        assert_eq!(theme.text, 0xc9d1d9);
        assert_eq!(theme.selected, 0x1f6feb);
        assert_eq!(theme.muted, 0x8b949e);
        assert_eq!(theme.border, 0x30363d);
        assert_eq!(theme.match_bg, 0x3d2200);
        assert_eq!(theme.selected_match_bg, 0x388bfd);
    }

    #[test]
    fn bundled_github_dark_file_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../themes/github-dark.json");
        let text = std::fs::read_to_string(path).expect("bundled theme");
        let theme = theme_from_json(&text).expect("parse");
        assert_eq!(theme.background, 0x0d1117);
        assert_eq!(theme.selected, 0x1f6feb);
    }
}
