//! Client-side daemon lifecycle: lazy race-free auto-start plus the helpers
//! behind `munin daemon {start,stop,status,restart}`.
//!
//! The `munin` CLI owns `munind`'s lifecycle (there is no systemd unit). Any
//! subcommand that needs the daemon calls [`ensure_running`] first, which
//! brings it up if the socket is dead. The capture/read/TUI paths never touch
//! this module — they stay strictly daemon-free.
//!
//! Race-freedom (`ensure_running`): try-connect → exclusive `flock` →
//! try-connect again (double-checked) → spawn detached → poll until the
//! socket accepts. The lock serializes concurrent spawners; the daemon's own
//! `bind_clean` (see `ipc.rs`) is the backstop if two ever race past it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use nix::fcntl::{Flock, FlockArg};
use tracing::{debug, info};

use crate::proto::{Request, Response};

/// How long to wait for a freshly-spawned daemon's socket to accept.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll cadence while waiting for the socket.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Try to connect to the daemon socket. `Some` means a daemon is alive.
pub fn try_connect(socket: &Path) -> Option<UnixStream> {
    UnixStream::connect(socket).ok()
}

/// Ensure a daemon is listening on `socket`, spawning one if not. Idempotent
/// and safe to call from many processes at once. Returns once the socket
/// accepts connections (or errors on timeout). `db` is forwarded to a
/// freshly-spawned daemon so a `--db` override is honoured (ignored if a
/// daemon is already up).
pub fn ensure_running(socket: &Path, db: Option<&Path>) -> Result<()> {
    if try_connect(socket).is_some() {
        return Ok(());
    }
    // Serialize spawners: whoever holds the lock is responsible for the spawn.
    let _lock = spawn_lock()?;
    // Double-check — another process may have started the daemon while we
    // blocked on the lock.
    if try_connect(socket).is_some() {
        return Ok(());
    }
    spawn_detached(socket, db)?;
    wait_for_socket(socket, SPAWN_TIMEOUT)
}

/// Acquire the exclusive spawn lock at `$XDG_RUNTIME_DIR/munin.spawn.lock`.
/// Held (via the returned guard) across spawn + readiness-poll, released on
/// drop. Blocks briefly if another spawner holds it.
fn spawn_lock() -> Result<Flock<std::fs::File>> {
    let path = levelup_core::xdg::runtime_file("munin.spawn.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open spawn lock {}", path.display()))?;
    Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, e)| anyhow!("lock {}: {e}", path.display()))
}

/// Spawn `munind` detached from this process and the controlling terminal:
/// `setsid` (new session, no TTY) and stdio redirected to the log file, so it
/// outlives the shell that triggered the spawn. Does not wait on the child.
fn spawn_detached(socket: &Path, db: Option<&Path>) -> Result<()> {
    let exe = munind_path()?;
    let log = open_log()?;
    let err = log.try_clone().context("clone log fd")?;

    let mut cmd = Command::new(&exe);
    // Pass through the resolved socket so a non-default `--socket` keeps the
    // CLI and the daemon it spawns in agreement.
    cmd.arg("--socket").arg(socket);
    // Honour a `--db` override so the spawned daemon uses the same database.
    if let Some(db) = db {
        cmd.arg("--db").arg(db);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    // SAFETY: setsid is async-signal-safe and we touch no other shared state.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map(|_| ()).map_err(Into::into)
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", exe.display()))?;
    info!(pid = child.id(), exe = %exe.display(), "spawned munind");
    Ok(())
}

/// Locate the `munind` binary: prefer one next to the running `munin`
/// executable (the `cargo install` / shared-prefix layout), else fall back to
/// a bare `munind` resolved via `PATH`.
fn munind_path() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("munind");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("munind"))
}

/// Open (creating + appending) the daemon log at
/// `$XDG_STATE_HOME/munin/munind.log`, ensuring its parent directory exists.
fn open_log() -> Result<std::fs::File> {
    let path = levelup_core::xdg::state_file("munin", "munind.log")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create log dir {}", dir.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open log {}", path.display()))
}

/// Poll-connect until the socket accepts or `timeout` elapses.
fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if try_connect(socket).is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let log = levelup_core::xdg::state_file("munin", "munind.log")
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "the daemon log".into());
            return Err(anyhow!(
                "munind did not come up within {timeout:?}; see {log}"
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Outcome of [`stop`].
pub enum Stopped {
    /// No daemon was running.
    NotRunning,
    /// The daemon acknowledged and its socket went away.
    Stopped,
}

/// Ask the running daemon to shut down and wait for its socket to disappear.
/// No-op (`NotRunning`) if nothing is listening.
pub fn stop(socket: &Path) -> Result<Stopped> {
    let Some(stream) = try_connect(socket) else {
        return Ok(Stopped::NotRunning);
    };
    match request(stream, &Request::Shutdown)? {
        Response::Ok => {}
        other => return Err(unexpected("shutdown", &other)),
    }
    // Wait for the daemon to actually exit (it removes the socket on the way
    // out). Reuse the spawn timeout as a generous bound.
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while try_connect(socket).is_some() {
        if Instant::now() >= deadline {
            return Err(anyhow!("munind acknowledged shutdown but is still listening"));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    debug!("munind stopped");
    Ok(Stopped::Stopped)
}

/// Liveness probe. `true` if a daemon answered `Ping`, `false` if nothing is
/// listening. Deliberately does **not** auto-start — a probe that starts the
/// thing it probes is surprising; use `ensure_running` / `daemon start` for
/// that.
pub fn ping(socket: &Path) -> Result<bool> {
    let Some(stream) = try_connect(socket) else {
        return Ok(false);
    };
    match request(stream, &Request::Ping)? {
        Response::Ok => Ok(true),
        other => Err(unexpected("ping", &other)),
    }
}

/// Query the running daemon's status. `None` if nothing is listening.
pub fn status(socket: &Path) -> Result<Option<Response>> {
    let Some(stream) = try_connect(socket) else {
        return Ok(None);
    };
    match request(stream, &Request::Status)? {
        resp @ Response::Status { .. } => Ok(Some(resp)),
        other => Err(unexpected("status", &other)),
    }
}

/// Send one request and read one line-delimited JSON response.
fn request(stream: UnixStream, req: &Request) -> Result<Response> {
    let mut reader = BufReader::new(stream.try_clone().context("clone socket")?);
    let mut writer = stream;
    let json = serde_json::to_string(req)?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    serde_json::from_str(line.trim()).with_context(|| format!("parse response {line:?}"))
}

fn unexpected(op: &str, resp: &Response) -> anyhow::Error {
    anyhow!("unexpected response to {op}: {resp:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// When something is already listening, `ensure_running` returns
    /// immediately without spawning (we'd hang/timeout if it tried to spawn a
    /// daemon at a bogus path). Binding a plain `UnixListener` is enough to
    /// make `try_connect` succeed.
    #[test]
    fn ensure_running_is_noop_when_socket_accepts() {
        let dir = std::env::temp_dir().join(format!("munin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("munin.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        // Must return Ok well under the spawn timeout, having spawned nothing.
        let start = Instant::now();
        ensure_running(&socket, None).unwrap();
        assert!(start.elapsed() < SPAWN_TIMEOUT);

        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir(&dir);
    }
}
