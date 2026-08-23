use std::fmt::Display;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::{Error, JsonStore, OldSettings};

static HARDCODED_DESCRIPTIONS: LazyLock<Vec<KeyBindingDescription>> =
    LazyLock::new(|| serde_json::from_str(include_str!("actions.json")).expect("Unable to load hardcoded actions"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Availability {
    WhenFocused,
    Always,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyBindingDescription {
    pub identifier: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub availability: Option<Availability>,
    pub default_bindings: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyBinding {
    pub identifier: String,
    pub binding: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyBindings(pub Vec<KeyBinding>);

/// Bundled autocomplete actions: identifier, availability, default bindings.
pub fn hardcoded_descriptions() -> &'static [KeyBindingDescription] {
    &HARDCODED_DESCRIPTIONS
}

/// `None` for private overlay actions such as `showAutocompleteFromTab`.
pub fn action_availability(identifier: &str) -> Option<Availability> {
    hardcoded_descriptions()
        .iter()
        .find(|description| description.identifier == identifier)
        .and_then(|description| description.availability)
}

/// PTY intercept while the overlay is hidden but still holding rows.
///
/// `Always` in `actions.json` is the WebView "works without overlay key
/// focus" bit — the panel is never activating. Once the list is parked, ESC
/// (`hideAutocomplete`) must reach the shell; intercepting it would steal
/// the second Escape after hide-until-shown / onlyShowOnTab.
pub fn intercepts_while_hidden(identifier: &str) -> bool {
    action_availability(identifier) == Some(Availability::Always) && identifier != "hideAutocomplete"
}

/// One entry per identifier that has a default binding, for figterm intercept.
pub fn default_action_bindings() -> Vec<(String, Vec<String>)> {
    hardcoded_descriptions()
        .iter()
        .filter_map(|description| {
            let bindings = description.default_bindings.as_ref()?;
            if bindings.is_empty() {
                return None;
            }
            Some((description.identifier.clone(), bindings.clone()))
        })
        .collect()
}

impl KeyBindings {
    pub fn load_hardcoded() -> Self {
        let key_bindings = hardcoded_descriptions()
            .iter()
            .flat_map(|description| {
                description.default_bindings.iter().flatten().map(|binding| KeyBinding {
                    identifier: description.identifier.clone(),
                    binding: binding.clone(),
                })
            })
            .collect();

        Self(key_bindings)
    }

    fn load_from_json_map(
        json_map: &serde_json::Map<String, serde_json::Value>,
        product_namespace: impl Display,
    ) -> Self {
        let key_bindings = json_map
            .into_iter()
            .filter_map(|(key, value)| {
                if let Some(key) = key.strip_prefix(&format!("{product_namespace}.keybindings.",)) {
                    Some(KeyBinding {
                        identifier: value.as_str()?.into(),
                        binding: key.into(),
                    })
                } else {
                    None
                }
            })
            .collect();
        Self(key_bindings)
    }

    pub fn load_from_settings(product_namespace: impl Display) -> Result<Self, Error> {
        let settings = OldSettings::load()?;
        let map = settings.map();
        Ok(Self::load_from_json_map(&map, product_namespace))
    }
}

impl IntoIterator for KeyBindings {
    type IntoIter = std::vec::IntoIter<Self::Item>;
    type Item = KeyBinding;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_load_json() {
        let json = KeyBindings::load_hardcoded();
        assert_eq!(json.0.len(), 24);

        assert_eq!(json.0[0].identifier, "insertSelected");
        assert_eq!(json.0[0].binding, "enter");
    }

    #[test]
    fn test_load_from_json_map() {
        let json_map = serde_json::json!({
            "autocomplete.keybindings.command+i": "toggleDescription",
            "autocomplete.keybindings.control+-": "increaseSize",
            "autocomplete.keybindings.control+/": "toggleDescription",
            "autocomplete.keybindings.control+=": "decreaseSize",
            "autocomplete.other": "other",
            "other": "other",
        })
        .as_object()
        .unwrap()
        .clone();

        let json = KeyBindings::load_from_json_map(&json_map, "autocomplete");

        assert_eq!(json.0.len(), 4);

        assert_eq!(json.0[0].identifier, "toggleDescription");
        assert_eq!(json.0[0].binding, "command+i");

        assert_eq!(json.0[1].identifier, "increaseSize");
        assert_eq!(json.0[1].binding, "control+-");

        assert_eq!(json.0[2].identifier, "toggleDescription");
        assert_eq!(json.0[2].binding, "control+/");

        assert_eq!(json.0[3].identifier, "decreaseSize");
        assert_eq!(json.0[3].binding, "control+=");
    }

    #[test]
    fn availability_matches_actions_json() {
        assert_eq!(action_availability("insertSelected"), Some(Availability::WhenFocused));
        assert_eq!(action_availability("hideAutocomplete"), Some(Availability::Always));
        assert_eq!(action_availability("showAutocomplete"), Some(Availability::Always));
        assert_eq!(action_availability("toggleAutocomplete"), Some(Availability::Always));
        assert_eq!(action_availability("showAutocompleteFromTab"), None);
    }

    #[test]
    fn hide_autocomplete_is_always_but_does_not_steal_esc_while_hidden() {
        assert!(intercepts_while_hidden("showAutocomplete"));
        assert!(intercepts_while_hidden("toggleAutocomplete"));
        assert!(!intercepts_while_hidden("hideAutocomplete"));
        assert!(!intercepts_while_hidden("insertSelected"));
        assert!(!intercepts_while_hidden("showAutocompleteFromTab"));
    }

    #[test]
    fn default_bindings_follow_actions_json_not_the_gpui_port_drift() {
        let bindings = default_action_bindings();
        let find = |id: &str| {
            bindings
                .iter()
                .find(|(identifier, _)| identifier == id)
                .map(|(_, keys)| keys.as_slice())
        };
        assert_eq!(find("selectSuggestion1"), Some(["control+1".to_string()].as_slice()));
        assert_eq!(find("toggleDescription"), Some(["command+i".to_string()].as_slice()));
        assert_eq!(
            find("navigateUp"),
            Some(
                [
                    "shift+tab".to_string(),
                    "up".to_string(),
                    "control+k".to_string(),
                    "control+p".to_string()
                ]
                .as_slice()
            )
        );
        assert_eq!(
            find("navigateDown"),
            Some(["down".to_string(), "control+j".to_string(), "control+n".to_string()].as_slice())
        );
        assert!(find("showAutocomplete").is_none());
        assert!(find("toggleAutocomplete").is_none());
    }
}
