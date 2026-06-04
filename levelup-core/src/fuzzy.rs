//! Thin nucleo-matcher helpers shared by every picker's search. The per-tool
//! scoring loop stays in each tool (item types and tiebreaks differ); what's
//! shared is the matcher/pattern construction and the `‹›` highlight wrapper.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// A fresh matcher with fzf-style defaults. Reuse one across a scoring pass.
pub fn matcher() -> Matcher {
    Matcher::new(Config::DEFAULT)
}

/// Parse a query into a pattern (`CaseMatching::Smart` + `Normalization::Smart`,
/// the fzf defaults).
pub fn pattern(query: &str) -> Pattern {
    Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
}

/// Walk `s` codepoint by codepoint, wrapping runs of matched positions
/// (`indices`, sorted ascending) in `‹…›`. The TUIs' `highlight_snippet` parses
/// these markers to colour the matched runs. Iterates `chars().enumerate()` so
/// multi-byte chars don't shift the markers.
pub fn highlight_indices(s: &str, indices: &[u32]) -> String {
    let mut out = String::with_capacity(s.len() + indices.len() * 4);
    let mut it = indices.iter().copied().peekable();
    let mut in_match = false;
    for (i, c) in s.chars().enumerate() {
        if it.peek() == Some(&(i as u32)) {
            if !in_match {
                out.push('‹');
                in_match = true;
            }
            out.push(c);
            it.next();
        } else {
            if in_match {
                out.push('›');
                in_match = false;
            }
            out.push(c);
        }
    }
    if in_match {
        out.push('›');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_wraps_contiguous_and_split_runs() {
        assert_eq!(highlight_indices("git commit", &[0, 1, 2]), "‹git› commit");
        assert_eq!(highlight_indices("abcd", &[0, 2]), "‹a›b‹c›d");
        // Multi-byte: index 3 is 'é'; the marker lands on the char, not bytes.
        assert_eq!(highlight_indices("café x", &[3]), "caf‹é› x");
    }
}
