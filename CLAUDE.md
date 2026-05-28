# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project vision

A Rust workspace of two personal-computing tools:

- **hugin** — Wayland clipboard manager for sway/wlroots compositors. Watches the clipboard via `wlr-data-control-unstable-v1` and persists every selection change. Eventual goals: expose clipboard history to AI via an MCP server, and synchronize between machines.
- **munin** — atuin-like shell history search with an fzf-style UI, fully customisable by the user. Currently a stub.

Both crates use `clap`, `thiserror`, `anyhow`, and `tracing` from `[workspace.dependencies]`.

## Workspace shape

Edition `2024`, resolver `"3"`. The workspace `Cargo.toml` at the repo root pins shared dependency versions; members inherit them with `.workspace = true`.

Quirk: **each member directory has its own nested `.git/`** — the workspace root itself is not a git repository. Stale per-crate `Cargo.lock` files (`hugin/Cargo.lock`, `munin/Cargo.lock`) may exist from before the workspace was set up; the authoritative lockfile is `./Cargo.lock`.

## Common commands

Run all from the workspace root.

```sh
cargo check  --workspace
cargo build  --workspace                        # debug
cargo build  --workspace --release
cargo clippy --workspace --all-targets
cargo fmt --all

cargo run --bin hugind    -p hugin              # run the daemon
cargo run --example schema -p hugin             # peek at the SQLite schema + row count

HUGIN_LOG=debug cargo run --bin hugind -p hugin # verbose logging via EnvFilter syntax
```

## hugin architecture

**Process model.** Single foreground binary `hugind`. No in-process daemonisation; supervised by `systemd --user` via [`dist/hugind.service`](dist/hugind.service) (`Type=notify`, gated on `graphical-session.target`, `Restart=on-failure`). Talks to systemd via the `sd-notify` crate: `READY=1` once IPC + wayland + storage are wired, `STOPPING=1` on SIGTERM/SIGINT. Outside systemd the notify calls are no-ops (NOTIFY_SOCKET unset).

**Shutdown.** `tokio::signal` listens for SIGTERM/SIGINT and flips an `Arc<AtomicBool>` that the wayland poll loop checks on every iteration (~50 ms worst-case wake latency, same budget as IPC commands). Graceful exit returns `Ok(())` → exit 0 → systemd does not restart. Wayland errors return `Err(_)` → exit non-zero → systemd retries while `graphical-session.target` is active.

**Threading.**
- *main thread* — wayland event loop (`event_queue.blocking_dispatch`). Pipe reads from `offer.receive(...)` happen inline; do not block this thread on disk or network.
- *`hugin-storage` thread* — owns the `rusqlite::Connection`. Receives `CapturedEntry` values via `std::sync::mpsc`, inserts them, runs hourly retention sweeps.

**Wayland protocol.** Raw `wayland-client` + `wayland-protocols-wlr` bindings to `zwlr_data_control_manager_v1` (versions 1–2). Wlroots-specific — works on sway, hyprland, river; **does not work on GNOME or KDE**. The design deliberately avoids `wl-clipboard-rs` so the protocol logic stays in-tree.

**Critical gotcha: self-mirror deadlock when we own the clipboard.** When `hugind` calls `set_selection` (i.e. serves a `hugin copy` request), the compositor mirrors the new selection back to *every* wlr-data-control client — including us. If we naively run our normal `handle_selection` over the echoed offer we will `receive(mime, fd)` against our own source, block on `read_to_end` of the pipe, and never get to dispatch the `Send` event that would write to that pipe's other end. The fix in `State::handle_selection` is to short-circuit when `self.sources` already contains an entry for that selection — drop the offer, don't try to read it. Forget this and `hugin copy` hangs forever and `wl-paste` blocks.

**Critical gotcha: fd ownership in `receive`.** In `wayland-protocols-wlr 0.3`, `ZwlrDataControlOfferV1::receive(mime, fd)` takes `BorrowedFd<'_>`, not `OwnedFd`. The correct pattern (see `read_offer` in `src/bin/hugind.rs`):

```rust
let (read_fd, write_fd) = pipe2(OFlag::O_CLOEXEC)?;
offer.receive(mime.to_string(), write_fd.as_fd());
drop(write_fd);              // close our write end so EOF reaches the read end
conn.flush()?;               // wayland has already dup'd the fd into the queued message
let mut file: std::fs::File = read_fd.into();
file.read_to_end(&mut buf)?;
```

The wayland library duplicates the fd into the request's ancillary data at the call site, so dropping our `OwnedFd` before flush is safe and necessary for EOF semantics. The same pattern will apply in reverse when wiring `data_source.send(mime, fd)` for `hugin copy`.

**Storage.**
- DB at `$XDG_DATA_HOME/hugin/hugin.db` (default `~/.local/share/hugin/hugin.db`). WAL mode + foreign keys on.
- Schema: `entries(id, ts_unix_ns, selection, hash, size_bytes, preview)` + `mime_parts(entry_id, mime, blob)` with `ON DELETE CASCADE`. Indexes on `hash`, `ts_unix_ns`, `(selection, id)`.
- **Dedup** is blake3 over canonical `(mime, blob)` parts sorted by MIME. A new capture is compared to the most recent entry *for the same selection*; identical → skipped, logged at `DEBUG`.
- **Retention** (`Store::maybe_retain`): at most once per hour. Deletes by age (default 90 days) and trims to most-recent N (default 10 000). Defaults in `RetentionConfig`.
- Storage is content-addressable on purpose, to keep the future cross-machine sync layer tractable.

**Two selections, two streams.** `Selection::Regular` = the CLIPBOARD selection (Ctrl-C / Ctrl-V), always watched. `Selection::Primary` = the PRIMARY selection (auto-populated by mouse text selection, pasted via middle-click), **off by default** because mouse-drag selection in many apps emits a steady stream of intermediate MIMEs that crowds the history. Enable with `hugind --primary`. Many apps populate both on Ctrl-C, so with `--primary` you'll often see two entries per copy.

**Logging.** `tracing` → stderr. `HUGIN_LOG` controls filter (full `tracing-subscriber` EnvFilter syntax). One `INFO stored …` line per persisted capture; `DEBUG` for dedup hits and selection-cleared events.

## munin architecture

Stub. `src/main.rs` prints `Hello, world!`. Will eventually be an atuin-style shell history search with an fzf-style TUI; will share workspace deps with hugin.

## Roadmap context

Hugin is being built in numbered milestones from a planning conversation. Quick orientation:
- **M0** (done) — log every clipboard change to stderr.
- **M1** (done) — SQLite persistence, dedup, retention, off-thread storage writes.
- **M2** (next) — `hugin` CLI + IPC. Daemon to serve a unix socket at `$XDG_RUNTIME_DIR/hugin.sock`. Wire protocol: JSON-lines for control + a raw-bytes trailer after a JSON header for `read-blob`. Subcommands: `list`, `get`, `copy`. This is where `tokio` is planned to enter the codebase.
- **M3** (done) — honours `x-kde-passwordManagerHint=secret` (the convention used by KeePassXC, Bitwarden, 1Password) and skips persisting such entries. Implemented in `State::handle_selection`: if the MIME list contains `x-kde-passwordManagerHint` and its content trims to `"secret"`, the whole offer is destroyed and no MIMEs are read.
- **M4** (done) — systemd user unit with `Type=notify`, graceful shutdown on SIGTERM/SIGINT, `--primary` flag (off by default, opt-in). Config file deferred to M5 (CLI flags + `Environment=`/drop-in in the unit are enough for now). Service file lives at [`dist/hugind.service`](dist/hugind.service); install steps in README.
- **Later** — MCP server exposing history to AI; cross-machine sync.
