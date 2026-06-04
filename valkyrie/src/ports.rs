//! Map listening TCP sockets to the PIDs that own them, by correlating
//! `/proc/net/{tcp,tcp6}` (socket inode → local port, LISTEN only) with each
//! process's `/proc/<pid>/fd/*` symlinks (`socket:[inode]`). This is what
//! `ss`/`lsof` do; kept in-tree (hugin's no-external-deps ethos).
//!
//! Only TCP `LISTEN` sockets are mapped — the "what's on :3000?" case — so the
//! haystack stays focused (UDP/ephemeral client ports would just be noise).
//! Other users' fds aren't readable without root, so their port attribution is
//! best-effort; that's surfaced honestly rather than hidden.

use std::collections::{HashMap, HashSet};
use std::fs;

/// `pid → sorted, de-duplicated listening ports`.
pub fn listening_by_pid() -> HashMap<i32, Vec<u16>> {
    let inode_to_port = collect_listening_inodes();
    if inode_to_port.is_empty() {
        return HashMap::new();
    }

    let mut by_pid: HashMap<i32, HashSet<u16>> = HashMap::new();
    let Ok(rd) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    for ent in rd.flatten() {
        let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        // EACCES for processes we don't own — skip quietly (best-effort).
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if let Ok(target) = fs::read_link(fd.path())
                && let Some(inode) = socket_inode(&target.to_string_lossy())
                && let Some(&port) = inode_to_port.get(&inode)
            {
                by_pid.entry(pid).or_default().insert(port);
            }
        }
    }

    by_pid
        .into_iter()
        .map(|(pid, set)| {
            let mut v: Vec<u16> = set.into_iter().collect();
            v.sort_unstable();
            (pid, v)
        })
        .collect()
}

fn collect_listening_inodes() -> HashMap<u64, u16> {
    let mut map = HashMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(s) = fs::read_to_string(file) {
            for line in s.lines().skip(1) {
                if let Some((inode, port)) = parse_listen_line(line) {
                    map.entry(inode).or_insert(port);
                }
            }
        }
    }
    map
}

/// One `/proc/net/tcp` row → `(inode, local_port)` if it's a LISTEN socket.
/// Columns: `sl local rem st … inode …` — local is `HEXIP:HEXPORT`, `st == 0A`
/// is LISTEN, inode is field 9.
fn parse_listen_line(line: &str) -> Option<(u64, u16)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 10 || f[3] != "0A" {
        return None;
    }
    let port = u16::from_str_radix(f[1].rsplit(':').next()?, 16).ok()?;
    let inode = f[9].parse::<u64>().ok()?;
    Some((inode, port))
}

/// `"socket:[12345]"` → `12345`.
fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listen_row_and_skips_others() {
        // 0100007F:1F90 = 127.0.0.1:8080, st 0A = LISTEN, inode 123456.
        let listen = "   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 \
                      00:00000000 00000000  1000        0 123456 1 0000 100 0 0 10 0";
        assert_eq!(parse_listen_line(listen), Some((123456, 8080)));

        // st 01 = ESTABLISHED → not a listener.
        let established = listen.replacen(" 0A ", " 01 ", 1);
        assert_eq!(parse_listen_line(&established), None);
    }

    #[test]
    fn socket_inode_parsing() {
        assert_eq!(socket_inode("socket:[98765]"), Some(98765));
        assert_eq!(socket_inode("/dev/null"), None);
        assert_eq!(socket_inode("anon_inode:[eventfd]"), None);
    }
}
