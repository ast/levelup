//! Turn the flat process snapshot into a collapsible parent/child forest.
//!
//! Used only for the *overview* (Ctrl-T, empty query) — searching always
//! returns a flat ranked list. The point is to tame sibling noise: a parent
//! whose children are mostly the *same program* (firefox's dozen content
//! processes) **auto-collapses** to a single line, and a collapsed node rolls
//! up its whole subtree's %CPU and memory so you see what the group costs.
//! Manual `expanded`/`collapsed` overrides win over the auto rule.

use std::collections::{HashMap, HashSet};

use crate::proc::Process;
use crate::search::Sort;

/// A parent auto-collapses once it has at least this many children …
const AUTO_COLLAPSE_MIN: usize = 4;
/// … and at least this fraction of them run the same program (argv0). This
/// folds firefox/alacritty-style homogeneous broods but leaves diverse parents
/// (init, your shell) expanded.
const DOMINANT_FRAC: f32 = 0.6;

/// Per-row tree decoration, parallel to the visible `Vec<Process>`.
#[derive(Debug, Clone)]
pub struct TreeMeta {
    pub depth: u16,
    /// `▾` expanded parent, `▸` collapsed parent, `None` leaf.
    pub marker: Option<char>,
    /// Hidden descendants when collapsed (0 otherwise).
    pub count: usize,
    /// %CPU to display: the subtree sum when collapsed, else the row's own.
    pub cpu_roll: f32,
    /// RSS (kB) to display: subtree sum when collapsed, else the row's own.
    pub rss_roll: u64,
}

impl TreeMeta {
    /// The trivial decoration for a flat (non-tree) row.
    pub fn flat(cpu: f32, rss: u64) -> Self {
        Self {
            depth: 0,
            marker: None,
            count: 0,
            cpu_roll: cpu,
            rss_roll: rss,
        }
    }
}

/// argv0 (the program), the signature we group siblings by — robust where
/// `comm` is truncated/renamed (firefox content procs share the binary path).
fn signature(p: &Process) -> &str {
    p.command.split_whitespace().next().unwrap_or(&p.command)
}

fn cmp(a: &Process, b: &Process, sort: Sort) -> std::cmp::Ordering {
    use std::cmp::Ordering::Equal;
    match sort {
        Sort::Cpu => b
            .cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(Equal)
            .then(b.rss_kb.cmp(&a.rss_kb)),
        Sort::Mem => b.rss_kb.cmp(&a.rss_kb),
        Sort::Pid => a.pid.cmp(&b.pid),
    }
}

/// Build the visible tree rows (pre-order DFS, collapsed subtrees skipped) and
/// their decorations, sorted by `sort` within each sibling group.
pub fn build(
    procs: &[Process],
    sort: Sort,
    expanded: &HashSet<i32>,
    collapsed: &HashSet<i32>,
) -> (Vec<Process>, Vec<TreeMeta>) {
    let by_pid: HashMap<i32, &Process> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        if p.ppid != p.pid {
            children.entry(p.ppid).or_default().push(p.pid);
        }
    }
    for kids in children.values_mut() {
        kids.sort_by(|&a, &b| cmp(by_pid[&a], by_pid[&b], sort));
    }

    // Roots: anything whose parent isn't in the snapshot (orphans, init, kthreadd).
    let mut roots: Vec<i32> = procs
        .iter()
        .filter(|p| p.ppid == p.pid || !by_pid.contains_key(&p.ppid))
        .map(|p| p.pid)
        .collect();
    roots.sort_by(|&a, &b| cmp(by_pid[&a], by_pid[&b], sort));

    let mut out = Vec::new();
    let mut meta = Vec::new();
    let mut visited = HashSet::new();
    // Iterative pre-order: push children reversed so they pop in sorted order.
    let mut stack: Vec<(i32, u16)> = roots.iter().rev().map(|&r| (r, 0)).collect();
    while let Some((pid, depth)) = stack.pop() {
        if !visited.insert(pid) {
            continue; // cycle guard
        }
        let Some(p) = by_pid.get(&pid) else { continue };
        let kids = children.get(&pid).map(|v| v.as_slice()).unwrap_or(&[]);
        let has_children = !kids.is_empty();
        let is_collapsed =
            has_children && effective_collapsed(pid, kids, &by_pid, expanded, collapsed);

        let (count, cpu_roll, rss_roll) = if is_collapsed {
            let (n, cpu, rss) = subtree_agg(pid, &children, &by_pid, &mut HashSet::new());
            (n.saturating_sub(1), cpu, rss)
        } else {
            (0, p.cpu_pct, p.rss_kb)
        };

        out.push((*p).clone());
        meta.push(TreeMeta {
            depth,
            marker: has_children.then_some(if is_collapsed { '▸' } else { '▾' }),
            count,
            cpu_roll,
            rss_roll,
        });

        if has_children && !is_collapsed {
            for &c in kids.iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }
    (out, meta)
}

fn effective_collapsed(
    pid: i32,
    kids: &[i32],
    by_pid: &HashMap<i32, &Process>,
    expanded: &HashSet<i32>,
    collapsed: &HashSet<i32>,
) -> bool {
    if collapsed.contains(&pid) {
        return true;
    }
    if expanded.contains(&pid) {
        return false;
    }
    auto_collapse(kids, by_pid)
}

fn auto_collapse(kids: &[i32], by_pid: &HashMap<i32, &Process>) -> bool {
    if kids.len() < AUTO_COLLAPSE_MIN {
        return false;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for &k in kids {
        if let Some(p) = by_pid.get(&k) {
            *counts.entry(signature(p)).or_default() += 1;
        }
    }
    let dominant = counts.values().copied().max().unwrap_or(0);
    dominant as f32 / kids.len() as f32 >= DOMINANT_FRAC
}

/// `(count_including_self, cpu_sum, rss_sum)` over a subtree.
fn subtree_agg(
    pid: i32,
    children: &HashMap<i32, Vec<i32>>,
    by_pid: &HashMap<i32, &Process>,
    visited: &mut HashSet<i32>,
) -> (usize, f32, u64) {
    if !visited.insert(pid) {
        return (0, 0.0, 0);
    }
    let Some(p) = by_pid.get(&pid) else {
        return (0, 0.0, 0);
    };
    let mut count = 1;
    let mut cpu = p.cpu_pct;
    let mut rss = p.rss_kb;
    if let Some(kids) = children.get(&pid) {
        for &c in kids {
            let (n, cc, cr) = subtree_agg(c, children, by_pid, visited);
            count += n;
            cpu += cc;
            rss += cr;
        }
    }
    (count, cpu, rss)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, cmd: &str) -> Process {
        Process {
            pid,
            ppid,
            comm: cmd.into(),
            command: cmd.into(),
            state: 'S',
            uid: 0,
            user: "u".into(),
            rss_kb: 10,
            threads: 1,
            cpu_pct: 1.0,
            ports: vec![],
            haystack: String::new(),
            snippet: None,
            cpu_jiffies: 0,
        }
    }

    #[test]
    fn homogeneous_brood_auto_collapses_with_rollup() {
        // init(1) → firefox(10) → 4 content procs (same argv0 "ff").
        let procs = vec![
            p(1, 0, "init"),
            p(10, 1, "ff"),
            p(11, 10, "ff"),
            p(12, 10, "ff"),
            p(13, 10, "ff"),
            p(14, 10, "ff"),
        ];
        let (rows, meta) = build(&procs, Sort::Pid, &HashSet::new(), &HashSet::new());
        let pids: Vec<i32> = rows.iter().map(|r| r.pid).collect();
        assert_eq!(
            pids,
            vec![1, 10],
            "the 4 children fold under the collapsed parent"
        );
        assert_eq!(meta[0].marker, Some('▾'), "init expanded (only 1 child)");
        assert_eq!(meta[1].marker, Some('▸'), "firefox collapsed");
        assert_eq!(meta[1].count, 4, "4 hidden descendants");
        assert!(
            (meta[1].cpu_roll - 5.0).abs() < 1e-6,
            "subtree cpu summed (5×1.0)"
        );
        assert_eq!(meta[1].rss_roll, 50, "subtree rss summed (5×10)");
    }

    #[test]
    fn diverse_parent_stays_expanded() {
        // init with 4 distinct-program children → no auto-collapse.
        let procs = vec![
            p(1, 0, "init"),
            p(2, 1, "a"),
            p(3, 1, "b"),
            p(4, 1, "c"),
            p(5, 1, "d"),
        ];
        let (rows, _) = build(&procs, Sort::Pid, &HashSet::new(), &HashSet::new());
        assert_eq!(rows.len(), 5, "all visible — diverse children don't fold");
    }

    #[test]
    fn manual_expand_overrides_auto() {
        let procs = vec![
            p(10, 1, "ff"),
            p(11, 10, "ff"),
            p(12, 10, "ff"),
            p(13, 10, "ff"),
            p(14, 10, "ff"),
        ];
        let expanded = HashSet::from([10]);
        let (rows, _) = build(&procs, Sort::Pid, &expanded, &HashSet::new());
        assert_eq!(rows.len(), 5, "force-expanded parent shows its children");
    }
}
