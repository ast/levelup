//! Syntax highlighting for the file preview pane — and *only* there.
//!
//! Latency discipline: highlighting never touches the list or the per-keystroke
//! match path. The picker holds one lazily-built `Highlighter` (the ~23ms
//! syntect dump load happens on the first *file* preview, then is cached for
//! the session), and we only ever colour the handful of lines the pane shows.
//! syntect runs on the pure-Rust `fancy-regex` engine (no oniguruma C build).

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::path::Path;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::util::sanitize_display;

pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    /// Build the engine: load the embedded syntax dump (no-newline variant,
    /// matching our line-stripped preview lines) and pick a dark default theme.
    /// Called once, lazily, via `OnceCell::get_or_init`.
    pub fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_nonewlines();
        let mut themes = ThemeSet::load_defaults();
        // base16-ocean.dark is a calm, widely-legible default; fall back to any
        // bundled theme if the set ever changes.
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.values().next().cloned())
            .expect("syntect ships at least one default theme");
        Self { syntaxes, theme }
    }

    /// Highlight `lines` (already capped to the visible preview height) for the
    /// file at `path`. Returns `None` when the language is unknown — the caller
    /// then renders the lines plain. Highlighting starts from line 0, which is
    /// correct because the preview always shows the file head.
    pub fn highlight(&self, path: &str, lines: &[String]) -> Option<Vec<Line<'static>>> {
        let syntax = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .or_else(|| {
                lines
                    .first()
                    .and_then(|l| self.syntaxes.find_syntax_by_first_line(l))
            })?;

        let mut h = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            // On any highlight error, bail to plain for the whole preview rather
            // than render a half-coloured pane.
            let ranges = h.highlight_line(line, &self.syntaxes).ok()?;
            let spans = ranges
                .into_iter()
                .map(|(style, text)| {
                    Span::styled(
                        sanitize_display(text),
                        Style::default().fg(to_ratatui(style.foreground)),
                    )
                })
                .collect::<Vec<_>>();
            out.push(Line::from(spans));
        }
        Some(out)
    }
}

/// syntect RGBA → ratatui colour. `a == 0` is syntect's "use the default
/// foreground" convention, so we leave the colour unset (terminal default).
fn to_ratatui(c: syntect::highlighting::Color) -> Color {
    if c.a == 0 {
        Color::Reset
    } else {
        Color::Rgb(c.r, c.g, c.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_into_coloured_spans() {
        let hl = Highlighter::new();
        let lines = vec!["fn main() {".to_string(), "    let x = 42;".to_string()];
        let out = hl
            .highlight("foo.rs", &lines)
            .expect("rust syntax found by .rs");
        assert_eq!(out.len(), 2, "one rendered Line per input line");
        // It actually coloured something: at least one span carries an Rgb fg.
        let coloured = out
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))));
        assert!(coloured, "expected at least one Rgb-coloured span");
    }

    #[test]
    fn unknown_language_returns_none() {
        let hl = Highlighter::new();
        // A made-up extension and an opaque first line → no syntax match → the
        // caller renders plain text.
        let lines = vec!["\u{1}\u{2}\u{3} blob".to_string()];
        assert!(hl.highlight("mystery.zzqq", &lines).is_none());
    }
}
