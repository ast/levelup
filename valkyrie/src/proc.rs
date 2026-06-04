//! Reading the process table out of `/proc`.
//!
//! A `Scanner` holds the small amount of state needed to compute **%CPU** as a
//! delta between two scans (per-process jiffies vs. total jiffies) and to cache
//! uid→username lookups. Each `scan()` returns a fresh `Vec<Process>`; the TUI
//! re-scans on a tick to show the world changing live.

use std::collections::HashMap;
use std::fs;

use nix::unistd::{Uid, User, getuid};

/// One process, as the picker needs it.
#[derive(Debug, Clone)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    /// Short name (`/proc/<pid>/comm`-style, from `stat`).
    pub comm: String,
    /// Full command line (argv joined), or `[comm]` for kernel threads.
    pub command: String,
    /// Scheduler state: `R`un, `S`leep, `D` uninterruptible, `T` stopped,
    /// `Z`ombie, `I`dle…
    pub state: char,
    pub uid: u32,
    pub user: String,
    pub rss_kb: u64,
    pub threads: u32,
    /// %CPU since the previous scan; `0.0` on the first scan (no delta yet).
    pub cpu_pct: f32,
    /// Listening ports owned by this process (filled in build-step 4).
    pub ports: Vec<u16>,
    /// What nucleo matches against: `comm command user pid :port…`.
    pub haystack: String,
    /// `‹…›`-marked `command` when a query matched it; else `None`.
    pub snippet: Option<String>,
    /// utime+stime in clock ticks — kept to diff against the next scan.
    cpu_jiffies: u64,
}

#[derive(Default)]
pub struct Scanner {
    prev_total: u64,
    prev_idle: u64,
    sys_cpu: f32,
    prev_proc: HashMap<i32, u64>,
    users: HashMap<u32, String>,
}

impl Scanner {
    /// Scan `/proc` once. %CPU is relative to the previous `scan()` call.
    pub fn scan(&mut self) -> Vec<Process> {
        let (total, idle) = read_cpu_totals();
        let total_delta = total.saturating_sub(self.prev_total);
        let idle_delta = idle.saturating_sub(self.prev_idle);
        let ports_map = crate::ports::listening_by_pid();

        let mut out = Vec::new();
        let mut new_proc = HashMap::new();
        if let Ok(rd) = fs::read_dir("/proc") {
            for ent in rd.flatten() {
                let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                    continue; // non-pid entries (e.g. /proc/self, /proc/meminfo)
                };
                // A process can vanish mid-scan — just skip unreadable ones.
                let Some(p) = self.read_proc(pid, total_delta, &ports_map) else {
                    continue;
                };
                new_proc.insert(pid, p.cpu_jiffies);
                out.push(p);
            }
        }
        self.prev_total = total;
        self.prev_idle = idle;
        self.sys_cpu = if total_delta > 0 {
            100.0 * total_delta.saturating_sub(idle_delta) as f32 / total_delta as f32
        } else {
            0.0
        };
        self.prev_proc = new_proc;
        out
    }

    fn read_proc(
        &mut self,
        pid: i32,
        total_delta: u64,
        ports_map: &HashMap<i32, Vec<u16>>,
    ) -> Option<Process> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (comm, state, ppid, jiffies) = parse_stat(&stat)?;
        let (uid, rss_kb, threads) = parse_status(pid);
        let command = read_cmdline(pid).unwrap_or_else(|| format!("[{comm}]"));

        let cpu_pct = if total_delta > 0 {
            // Unknown previous sample (new process) → treat as no movement.
            let prev = self.prev_proc.get(&pid).copied().unwrap_or(jiffies);
            100.0 * jiffies.saturating_sub(prev) as f32 / total_delta as f32
        } else {
            0.0
        };

        let user = self.user_for(uid);
        let ports = ports_map.get(&pid).cloned().unwrap_or_default();

        let mut haystack = format!("{comm} {command} {user} {pid}");
        for port in &ports {
            haystack.push_str(" :");
            haystack.push_str(&port.to_string());
        }

        Some(Process {
            pid,
            ppid,
            comm,
            command,
            state,
            uid,
            user,
            rss_kb,
            threads,
            cpu_pct,
            ports,
            haystack,
            snippet: None,
            cpu_jiffies: jiffies,
        })
    }

    fn user_for(&mut self, uid: u32) -> String {
        if let Some(name) = self.users.get(&uid) {
            return name.clone();
        }
        let name = User::from_uid(Uid::from_raw(uid))
            .ok()
            .flatten()
            .map(|u| u.name)
            .unwrap_or_else(|| uid.to_string());
        self.users.insert(uid, name.clone());
        name
    }

    /// System-wide busy CPU% from the last two scans (0 on the first scan).
    pub fn system_cpu(&self) -> f32 {
        self.sys_cpu
    }
}

/// The real uid of whoever's running us — for the "show only mine" scope.
pub fn current_uid() -> u32 {
    getuid().as_raw()
}

/// The 1/5/15-minute load averages from `/proc/loadavg`.
pub fn load_avg() -> (f32, f32, f32) {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| {
            let mut it = s.split_whitespace();
            Some((
                it.next()?.parse().ok()?,
                it.next()?.parse().ok()?,
                it.next()?.parse().ok()?,
            ))
        })
        .unwrap_or((0.0, 0.0, 0.0))
}

/// Parse `/proc/<pid>/stat` → `(comm, state, ppid, utime+stime)`. The `comm`
/// field is parenthesised and may itself contain spaces and parens, so we split
/// on the *last* `)` and index the remaining whitespace-separated fields (which
/// begin at field 3, `state`).
fn parse_stat(s: &str) -> Option<(String, char, i32, u64)> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest = s.get(close + 1..)?.trim_start();
    let f: Vec<&str> = rest.split_whitespace().collect();
    // rest[0] = field 3 (state); rest[k] = field (3+k).
    let state = f.first()?.chars().next()?;
    let ppid: i32 = f.get(1)?.parse().ok()?;
    let utime: u64 = f.get(11)?.parse().ok()?; // field 14
    let stime: u64 = f.get(12)?.parse().ok()?; // field 15
    Some((comm, state, ppid, utime + stime))
}

/// Pull the real uid, RSS (kB), and thread count out of `/proc/<pid>/status`.
/// Kernel threads have no `VmRSS` line → 0.
fn parse_status(pid: i32) -> (u32, u64, u32) {
    let mut uid = 0;
    let mut rss_kb = 0;
    let mut threads = 1;
    if let Ok(s) = fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in s.lines() {
            if let Some(r) = line.strip_prefix("Uid:") {
                uid = r
                    .split_whitespace()
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);
            } else if let Some(r) = line.strip_prefix("VmRSS:") {
                rss_kb = r
                    .split_whitespace()
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);
            } else if let Some(r) = line.strip_prefix("Threads:") {
                threads = r
                    .split_whitespace()
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(1);
            }
        }
    }
    (uid, rss_kb, threads)
}

/// Full command line from `/proc/<pid>/cmdline` (NUL-separated argv). `None`
/// for kernel threads / empty cmdline (caller falls back to `[comm]`).
fn read_cmdline(pid: i32) -> Option<String> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let parts: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// `(total, idle)` jiffies from the aggregate `cpu` line of `/proc/stat`.
/// `total` is the %CPU denominator; `idle` (idle + iowait) lets us derive the
/// system-wide busy% for the detail strip's calm bar.
fn read_cpu_totals() -> (u64, u64) {
    let parse = || -> Option<(u64, u64)> {
        let s = fs::read_to_string("/proc/stat").ok()?;
        let rest = s.lines().next()?.strip_prefix("cpu ")?;
        let nums: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        let total: u64 = nums.iter().sum();
        let idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
        Some((total, idle))
    };
    parse().unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_handles_comm_with_spaces_and_parens() {
        // comm = "(sd-pam)" style: spaces and parens inside the parens must not
        // break field indexing (we split on the LAST ')').
        let line = "1234 (weird )(name) S 1000 1234 1234 0 -1 4194560 100 0 0 0 \
                    42 8 0 0 20 0 1 0 999 12345678 200 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let (comm, state, ppid, jiffies) = parse_stat(line).unwrap();
        assert_eq!(comm, "weird )(name");
        assert_eq!(state, 'S');
        assert_eq!(ppid, 1000);
        assert_eq!(jiffies, 42 + 8); // utime + stime
    }
}
