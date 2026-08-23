use std::borrow::Cow;
use std::fmt;

pub const SETTINGS_ID: WindowId = WindowId(Cow::Borrowed("settings"));
pub const AUTOCOMPLETE_ID: WindowId = WindowId(Cow::Borrowed("autocomplete"));

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowId(pub Cow<'static, str>);

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for WindowId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_window_id_is_not_the_webview_dashboard() {
        assert_eq!(SETTINGS_ID.0.as_ref(), "settings");
        assert_eq!(AUTOCOMPLETE_ID.0.as_ref(), "autocomplete");
        assert_ne!(SETTINGS_ID, AUTOCOMPLETE_ID);
    }
}
