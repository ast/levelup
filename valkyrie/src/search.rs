//! Fuzzy ranking of the process snapshot (nucleo), and the sort modes.
//!
//! We score against each process's rich `haystack` (so typing a PID, user, or
//! `:port` filters), but highlight only the `command` (so the row shows where
//! the match landed). Empty query → no scoring, just the chosen sort.

use std::cmp::Ordering;

use crate::proc::Process;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Cpu,
    Mem,
    Pid,
}

impl Sort {
    pub fn cycle(self) -> Self {
        match self {
            Sort::Cpu => Sort::Mem,
            Sort::Mem => Sort::Pid,
            Sort::Pid => Sort::Cpu,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Mem => "mem",
            Sort::Pid => "pid",
        }
    }
}

fn sort_procs(procs: &mut [Process], sort: Sort) {
    match sort {
        Sort::Cpu => procs.sort_by(|a, b| {
            b.cpu_pct
                .partial_cmp(&a.cpu_pct)
                .unwrap_or(Ordering::Equal)
                .then(b.rss_kb.cmp(&a.rss_kb))
        }),
        Sort::Mem => procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb)),
        Sort::Pid => procs.sort_by_key(|p| p.pid),
    }
}

/// Rank `procs` for `query`. Empty query → sorted by `sort`. Non-empty →
/// nucleo over the haystack (match score desc, then by the active sort key as
/// tiebreak); non-matches dropped; the command gets `‹…›` highlights.
pub fn rank(mut procs: Vec<Process>, query: &str, sort: Sort) -> Vec<Process> {
    let q = query.trim();
    if q.is_empty() {
        sort_procs(&mut procs, sort);
        return procs;
    }

    // `:PORT` is an *exact* port query, not a fuzzy one. Without this, `:8000`
    // fuzzily matches any long command line containing a `:` and the digits
    // 8,0,0,0 scattered about (browser cmdlines, say). A port is a precise
    // concept — treat it precisely.
    if let Some(port) = parse_port_query(q) {
        procs.retain(|p| p.ports.contains(&port));
        sort_procs(&mut procs, sort);
        return procs;
    }

    let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
    let pattern = nucleo_matcher::pattern::Pattern::parse(
        q,
        nucleo_matcher::pattern::CaseMatching::Smart,
        nucleo_matcher::pattern::Normalization::Smart,
    );

    let mut hay = Vec::new();
    let mut cmd = Vec::new();
    let mut idx = Vec::new();
    let mut scored: Vec<(u32, Process)> = Vec::with_capacity(procs.len());
    for mut p in procs {
        idx.clear();
        let h = nucleo_matcher::Utf32Str::new(&p.haystack, &mut hay);
        let Some(score) = pattern.indices(h, &mut matcher, &mut idx) else {
            continue;
        };
        // Highlight only the command (the match may have landed on pid/user/
        // port instead, in which case the command shows no markers — fine).
        idx.clear();
        let c = nucleo_matcher::Utf32Str::new(&p.command, &mut cmd);
        if pattern.indices(c, &mut matcher, &mut idx).is_some() {
            idx.sort_unstable();
            p.snippet = Some(highlight_indices(&p.command, &idx));
        }
        scored.push((score, p));
    }

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| match sort {
            Sort::Cpu => {
                b.1.cpu_pct
                    .partial_cmp(&a.1.cpu_pct)
                    .unwrap_or(Ordering::Equal)
            }
            Sort::Mem => b.1.rss_kb.cmp(&a.1.rss_kb),
            Sort::Pid => a.1.pid.cmp(&b.1.pid),
        })
    });
    scored.into_iter().map(|(_, p)| p).collect()
}

/// A `:PORT` query → the port number, for exact filtering. Plain digits (no
/// colon) stay general fuzzy queries (they could be a PID or part of a name).
fn parse_port_query(q: &str) -> Option<u16> {
    let digits = q.strip_prefix(':')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Wrap matched codepoint runs in `‹…›` (same marker convention as the sibling
/// crates' TUIs). Iterates `chars().enumerate()` so multi-byte chars don't
/// shift the markers.
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
    fn highlight_wraps_runs() {
        assert_eq!(highlight_indices("abcd", &[0, 2]), "‹a›b‹c›d");
    }

    #[test]
    fn port_query_is_exact_only_with_colon() {
        assert_eq!(parse_port_query(":8000"), Some(8000));
        assert_eq!(parse_port_query(":80"), Some(80));
        assert_eq!(parse_port_query("8000"), None); // no colon → general fuzzy
        assert_eq!(parse_port_query(":"), None);
        assert_eq!(parse_port_query(":80a"), None);
    }
}
