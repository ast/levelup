//! The swaybar / i3bar JSON protocol.
//!
//! Output: a one-line header object, then `[`, then an infinite stream of
//! "status line" arrays (one array of [`Block`]s per render), each followed
//! by a comma. swaybar tolerates the trailing comma.
//!
//! Input (when the header sets `click_events: true`): a `[` followed by a
//! stream of [`ClickEvent`] objects, comma-separated, one per line.

use serde::{Deserialize, Serialize};

/// The protocol header. Emitted once, before the opening `[` of the body.
pub const HEADER_LINE: &str = r#"{"version":1,"click_events":true}"#;

/// One renderable block on the bar. Only the fields mimir actually sets are
/// modelled; absent optionals are skipped so the JSON stays compact.
#[derive(Debug, Clone, Serialize)]
pub struct Block {
    /// The text shown on the bar (may contain Pango markup if `markup` is set).
    pub full_text: String,
    /// Shorter fallback swaybar uses when the bar is too narrow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_text: Option<String>,
    /// Foreground colour as `#rrggbb`. `None` → swaybar's theme default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Logical block name; echoed back in a [`ClickEvent`] so we can route it.
    pub name: String,
    /// Distinguishes multiple blocks sharing a `name` (e.g. one per mount).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// `"pango"` to enable markup, omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markup: Option<String>,
    /// Ask swaybar to render this block with the urgent style.
    #[serde(skip_serializing_if = "is_false")]
    pub urgent: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A click delivered on stdin by swaybar. We only model the fields we use;
/// `serde` ignores the rest (x/y/width/relative coords/modifiers).
#[derive(Debug, Clone, Deserialize)]
pub struct ClickEvent {
    /// The `name` of the clicked block.
    #[serde(default)]
    pub name: Option<String>,
    /// The `instance` of the clicked block, if it set one.
    #[serde(default)]
    pub instance: Option<String>,
    /// Mouse button: 1=left, 2=middle, 3=right, 4=scroll-up, 5=scroll-down.
    pub button: u8,
}

impl ClickEvent {
    pub fn is_scroll(&self) -> bool {
        matches!(self.button, 4 | 5)
    }
}

/// Escape a string for safe inclusion in Pango markup. Only the three
/// characters that are significant to the markup parser need escaping; we do
/// this on every dynamic value when `markup = "pango"` so an SSID or path
/// containing `&`/`<`/`>` can't corrupt the bar.
pub fn pango_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_serializes_compactly() {
        let b = Block {
            full_text: "CPU 12%".into(),
            short_text: None,
            color: Some("#ffffff".into()),
            name: "cpu".into(),
            instance: None,
            markup: None,
            urgent: false,
        };
        let json = serde_json::to_string(&b).unwrap();
        // No short_text / instance / markup / urgent keys when unset.
        assert_eq!(
            json,
            r##"{"full_text":"CPU 12%","color":"#ffffff","name":"cpu"}"##
        );
    }

    #[test]
    fn click_event_parses_minimal() {
        let ev: ClickEvent =
            serde_json::from_str(r#"{"name":"clock","button":1,"x":10,"y":4}"#).unwrap();
        assert_eq!(ev.name.as_deref(), Some("clock"));
        assert_eq!(ev.button, 1);
        assert!(!ev.is_scroll());
        let scroll: ClickEvent = serde_json::from_str(r#"{"button":5}"#).unwrap();
        assert!(scroll.is_scroll());
        assert!(scroll.name.is_none());
    }

    #[test]
    fn pango_escape_handles_specials() {
        assert_eq!(pango_escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }
}
