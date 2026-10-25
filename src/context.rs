//! The plugin invocation context herdr passes in `HERDR_PLUGIN_CONTEXT_JSON`.

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Context {
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub tab_label: Option<String>,
    pub focused_pane_id: Option<String>,
}

impl Context {
    pub fn from_env() -> Context {
        std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_fields_herdr_sends() {
        // Captured from herdr 0.9.5.
        let json = r#"{"workspace_id":"wA","workspace_label":"herdr","tab_id":"wA:t1C",
            "tab_label":"5","focused_pane_id":"wA:p2D","focused_pane_agent":"claude",
            "invocation_source":"cli"}"#;
        let context: Context = serde_json::from_str(json).unwrap();
        assert_eq!(context.workspace_id.as_deref(), Some("wA"));
        assert_eq!(context.tab_id.as_deref(), Some("wA:t1C"));
        assert_eq!(context.focused_pane_id.as_deref(), Some("wA:p2D"));
    }
}
