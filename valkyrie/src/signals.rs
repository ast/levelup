//! Sending signals, and the curated signal set for the chooser.
//!
//! Signaling goes through `nix::sys::signal::kill`. The result is mapped to a
//! small enum so the picker can say something honest in the status line —
//! "needs root" (EPERM), "gone" (ESRCH) — rather than a raw errno.

use std::str::FromStr;

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// One entry in the curated signal chooser.
pub struct SignalSpec {
    pub sig: Signal,
    pub desc: &'static str,
}

/// The signals you actually reach for, in a sensible order. The chooser shows
/// these by default; you can still type any other name/number.
pub const CURATED: &[SignalSpec] = &[
    SignalSpec {
        sig: Signal::SIGTERM,
        desc: "graceful terminate (default)",
    },
    SignalSpec {
        sig: Signal::SIGKILL,
        desc: "force kill (uncatchable)",
    },
    SignalSpec {
        sig: Signal::SIGSTOP,
        desc: "pause (reversible)",
    },
    SignalSpec {
        sig: Signal::SIGCONT,
        desc: "resume",
    },
    SignalSpec {
        sig: Signal::SIGHUP,
        desc: "hangup / reload config",
    },
    SignalSpec {
        sig: Signal::SIGINT,
        desc: "interrupt (Ctrl-C)",
    },
    SignalSpec {
        sig: Signal::SIGQUIT,
        desc: "quit + core dump",
    },
    SignalSpec {
        sig: Signal::SIGUSR1,
        desc: "user-defined 1",
    },
    SignalSpec {
        sig: Signal::SIGUSR2,
        desc: "user-defined 2",
    },
];

/// Outcome of trying to signal a process, in terms the user cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendResult {
    Sent,
    /// EPERM — not ours to signal (needs root).
    NeedRoot,
    /// ESRCH — already gone.
    Gone,
    Err(String),
}

pub fn send(pid: i32, sig: Signal) -> SendResult {
    match kill(Pid::from_raw(pid), sig) {
        Ok(()) => SendResult::Sent,
        Err(Errno::EPERM) => SendResult::NeedRoot,
        Err(Errno::ESRCH) => SendResult::Gone,
        Err(e) => SendResult::Err(e.to_string()),
    }
}

/// Parse a user-typed signal: a number (`9`, `15`), or a name with or without
/// the `SIG` prefix, case-insensitive (`kill`, `SIGKILL`, `term`).
pub fn parse_signal(s: &str) -> Option<Signal> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<i32>() {
        return Signal::try_from(n).ok();
    }
    let mut name = s.to_ascii_uppercase();
    if !name.starts_with("SIG") {
        name.insert_str(0, "SIG");
    }
    Signal::from_str(&name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_by_number_name_and_bare_name() {
        assert_eq!(parse_signal("9"), Some(Signal::SIGKILL));
        assert_eq!(parse_signal("15"), Some(Signal::SIGTERM));
        assert_eq!(parse_signal("SIGTERM"), Some(Signal::SIGTERM));
        assert_eq!(parse_signal("term"), Some(Signal::SIGTERM));
        assert_eq!(parse_signal("HUP"), Some(Signal::SIGHUP));
        assert_eq!(parse_signal("nonsense"), None);
        assert_eq!(parse_signal(""), None);
    }
}
