//! Render the `‹›` markers produced by `levelup_core::fuzzy::highlight_indices`
//! into ratatui spans.

use levelup_core::sanitize_display;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Parse `‹›` markers: matched runs get `match_fg` (bold), everything else
/// `base` (pass `Color::Reset` for the terminal default). Control bytes are
/// neutralised first via `sanitize_display` (which also turns newlines into `↵`
/// so a multi-line snippet stays on one row); it never emits the markers, so
/// the parse stays correct.
pub fn highlight_snippet(s: &str, match_fg: Color, base: Color) -> Vec<Span<'static>> {
    let s = sanitize_display(s);
    let base_style = Style::default().fg(base);
    let match_style = Style::default().fg(match_fg).add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut in_match = false;
    for ch in s.chars() {
        match ch {
            '‹' => {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), base_style));
                }
                in_match = true;
            }
            '›' => {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), match_style));
                }
                in_match = false;
            }
            c => buf.push(c),
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(
            buf,
            if in_match { match_style } else { base_style },
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_into_match_and_base_runs() {
        let spans = highlight_snippet("‹git› commit", Color::Yellow, Color::Reset);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "git");
        assert_eq!(spans[0].style.fg, Some(Color::Yellow));
        assert_eq!(spans[1].content, " commit");
        assert_eq!(spans[1].style.fg, Some(Color::Reset));
    }
}
