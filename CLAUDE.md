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
- Schema: `entries(id, ts_unix_ns, selection, hash, size_bytes)` + `mime_parts(entry_id, mime, blob)` with `ON DELETE CASCADE`. Indexes on `hash`, `ts_unix_ns`, `(selection, id)`. Plus the `entries_fts` virtual table (see below).
- **Dedup** is blake3 over canonical `(mime, blob)` parts sorted by MIME. A new capture is compared to the most recent entry *for the same selection*; identical → skipped, logged at `DEBUG`.
- **Retention** (`Store::maybe_retain`): at most once per hour. Deletes by age (default 90 days) and trims to most-recent N (default 10 000). Defaults in `RetentionConfig`.
- Storage is content-addressable on purpose, to keep the future cross-machine sync layer tractable.

**Full-text search (FTS5).**
- `entries_fts` is an FTS5 virtual table holding one `content` column. Full-content mode — text is stored a second time inside the FTS index; this is intentional, traded against the smaller external-content variant for simpler invariants.
- Indexing happens inside the same transaction as the `entries`/`mime_parts` insert. `pick_indexable_text` is a tiered single-pass picker: `text/plain` (UTF-8) > other `text/*` (UTF-8) > any text-MIME via lossy UTF-8 decode. The lossy tier exists so legacy X11 atoms like `STRING` (Latin-1) still land in the index — non-UTF-8 bytes become U+FFFD, which terminates the matching token early but keeps the surrounding tokens searchable.
- `AFTER DELETE` trigger `entries_ad` deletes the matching FTS row whenever an `entries` row is removed (retention or otherwise). The FK CASCADE on `mime_parts` doesn't reach the virtual table, so the trigger is load-bearing.
- `EntryMeta.snippet` (note: not `preview`) is populated from `substr(entries_fts.content, 1, 200)` for `list`/`get`, and from `snippet(entries_fts, 0, '‹', '›', '…', 16)` for `search`. Same field, two contents: a leading text excerpt or a highlight-marked excerpt. The CLI's `print_table` balances unclosed `‹` markers after truncation.
- `list` and `search` clamp their `limit` to `MAX_LIMIT` (10 000) before binding it. SQLite treats a negative LIMIT as unlimited, and `usize::MAX as i64` wraps to -1 — without the clamp any IPC peer could ask the daemon to materialise the entire table.
- `search` with an empty/whitespace query short-circuits to `Ok(vec![])` instead of forwarding `""` to FTS5 (which would otherwise raise a syntax error).

**Schema versioning.** `PRAGMA user_version` carries the schema generation (current: `2` = FTS landed). `Store::open` calls `ensure_compatible_schema` *before* setting any pragmas (so a rejected DB doesn't leave `-wal`/`-shm` sidecars), then applies `SCHEMA_SQL` + the version stamp inside a single transaction (so a crash between them can't strand a half-stamped DB the next start would reject):
- `user_version == DB_VERSION` + `entries_fts` present → OK.
- `user_version == DB_VERSION` + `entries_fts` missing → refuse; the FTS index has been dropped out-of-band and silently rebuilding it empty would hide all historical rows from search.
- `user_version == 0` and no `entries` table → fresh DB; apply schema, set version.
- `user_version == 0` with an existing `entries` table → pre-FTS DB; refuse to start with a message naming the file. No automatic migration — the user deletes the file and the daemon recreates it.
- Any other version → refuse, same way.

**Two selections, two streams.** `Selection::Regular` = the CLIPBOARD selection (Ctrl-C / Ctrl-V), always watched. `Selection::Primary` = the PRIMARY selection (auto-populated by mouse text selection, pasted via middle-click), **off by default** because mouse-drag selection in many apps emits a steady stream of intermediate MIMEs that crowds the history. Enable with `hugind --primary`. Many apps populate both on Ctrl-C, so with `--primary` you'll often see two entries per copy.

**Logging.** `tracing` → stderr. `HUGIN_LOG` controls filter (full `tracing-subscriber` EnvFilter syntax). One `INFO stored …` line per persisted capture; `DEBUG` for dedup hits and selection-cleared events.

## munin architecture

Stub at this writing — `src/main.rs` prints `Hello, world!`. What follows is the **planned** shape; sections will harden into "is" statements as milestones land.

**Process model.** No long-running daemon. Unlike hugin there is nothing to watch in the background — the shell itself fires events at well-defined moments (command about to run, command just finished, history-search keybind pressed). Each event invokes a short-lived `munin` process. SQLite WAL mode makes the concurrent add-from-shell + read-from-TUI case safe. Startup latency is a hard constraint: hooks run on every prompt, so cold start must stay well under 50 ms or the shell feels laggy. Revisit the daemon question if/when sync arrives.

**Shell integration.** Shell-specific glue is intentionally thin and lives next to the binary, not inside it. A `shells/` directory ships zsh and bash hook scripts (~30 lines each) that funnel into the same Rust-side surface:

- `munin add --start <cmd>` on `preexec` — records command + start time + cwd + session, prints the row id on stdout.
- `munin add --end <id> --exit <code>` on `precmd` — closes the row out with duration and exit code.
- `munin search --interactive` for the Ctrl-R keybind — opens the TUI, prints the selected command to stdout for the hook to splice into the line buffer.

`munin init <shell>` prints the matching hook script so users wire it up with `eval "$(munin init zsh)"`. Adding fish (or nushell, or anything else) later is one more script in `shells/` plus one arm in `init` — the Rust core doesn't change.

**Storage.** DB at `$XDG_DATA_HOME/munin/munin.db` (default `~/.local/share/munin/munin.db`). WAL mode, foreign keys on, same conventions as hugin's store. Initial schema sketch:

- `entries(id INTEGER PRIMARY KEY, cmd TEXT NOT NULL, ts_unix_ns INTEGER NOT NULL, cwd TEXT, hostname TEXT, session TEXT, shell TEXT, exit_code INTEGER, duration_ms INTEGER)`
- Indexes on `ts_unix_ns`, `(session, ts_unix_ns)`, `cwd`.
- FTS5 virtual table on `cmd` lands in M3; same full-content + `AFTER DELETE` trigger pattern hugin uses.

No insert-time dedup — repeated commands are the norm and timing data is interesting per-invocation. The TUI deduplicates the *view* (most-recent-first, hide repeats), atuin-style.

**Filtering.** Lines beginning with whitespace are skipped at capture time, matching the existing shell convention (zsh `HIST_IGNORE_SPACE`, bash `HISTCONTROL=ignorespace`). Regex denylist via config is M6.

**Logging.** `tracing` to stderr, gated by `MUNIN_LOG` (full EnvFilter syntax). Default level is silent — hook invocations run on every prompt and noise matters.

**Workspace fit.** Already uses `clap`, `thiserror`, `anyhow`, `tracing`, `tracing-subscriber` from `[workspace.dependencies]`. `rusqlite` joins the workspace deps in M1; `ratatui` + `crossterm` in M4.

## Roadmap context

### Hugin

Hugin is being built in numbered milestones from a planning conversation. Quick orientation:
- **M0** (done) — log every clipboard change to stderr.
- **M1** (done) — SQLite persistence, dedup, retention, off-thread storage writes.
- **M2** (next) — `hugin` CLI + IPC. Daemon to serve a unix socket at `$XDG_RUNTIME_DIR/hugin.sock`. Wire protocol: JSON-lines for control + a raw-bytes trailer after a JSON header for `read-blob`. Subcommands: `list`, `get`, `copy`. This is where `tokio` is planned to enter the codebase.
- **M3** (done) — honours `x-kde-passwordManagerHint=secret` (the convention used by KeePassXC, Bitwarden, 1Password) and skips persisting such entries. Implemented in `State::handle_selection`: if the MIME list contains `x-kde-passwordManagerHint` and its content trims to `"secret"`, the whole offer is destroyed and no MIMEs are read.
- **M4** (done) — systemd user unit with `Type=notify`, graceful shutdown on SIGTERM/SIGINT, `--primary` flag (off by default, opt-in). Config file deferred to M5 (CLI flags + `Environment=`/drop-in in the unit are enough for now). Service file lives at [`dist/hugind.service`](dist/hugind.service); install steps in README.
- **M5** (done) — FTS5 full-text search. `hugin search <query>` (alias `s`) with `--raw`, `--sort=relevance|recent` (default relevance), `--limit`, `--selection`. Schema bumped to v2; pre-FTS DBs are not migrated, daemon refuses to start until the user deletes the file.
- **Later** — MCP server exposing history to AI; cross-machine sync.

### Munin

Numbered milestones, same convention. Nothing built yet.

- **M0** (next) — minimum capture loop. `munin init zsh` / `munin init bash` print hook scripts. `munin add --start <cmd>` and `munin add --end <id> --exit <code>` exist as no-op shims that log the captured fields to stderr (no DB yet). Goal: prove the shell→binary glue and confirm both shells fire the hooks we expect, before any storage code.
- **M1** — SQLite persistence. `Store::open` + `Store::add_start` / `Store::add_end`. Schema above, WAL mode, indexes. Skip lines starting with whitespace. `PRAGMA user_version = 1`. No retention sweep yet — shell history scales to millions of rows comfortably; revisit only if it bites.
- **M2** — non-interactive CLI. `munin list` (most recent N), `munin search <query>` (LIKE-based for now, no FTS), `munin import` for `.zsh_history` + `.bash_history` (parses bash's `HISTTIMEFORMAT` extended format when present). Filters: `--limit`, `--cwd`, `--session`, `--shell`. Output: TSV by default for piping, `--human` for a readable column layout.
- **M3** — FTS5 search. Mirror hugin's M5 closely: full-content FTS5 table on `cmd`, `AFTER DELETE` trigger, `--sort=relevance|recent` (default relevance), `snippet(...)` highlighting with `‹›` markers, schema bump to `user_version = 2`. Same "refuse to start, name the file, user deletes it" policy for pre-FTS DBs.
- **M4** — interactive TUI. `ratatui` + `crossterm`, fzf-style: search-as-you-type, arrow nav, enter prints the selected command to stdout. Customisable theme + keybindings via `$XDG_CONFIG_HOME/munin/config.toml`. Customisability is a core goal of munin (see project vision), so the config schema lands here even if the default keymap is just enough to be usable.
- **M5** — shell binding. `munin init <shell>` output now includes a Ctrl-R binding that runs `munin search --interactive` and splices the chosen command into the line buffer (zsh: `BUFFER=…; zle reset-prompt`; bash: `READLINE_LINE=…; READLINE_POINT=…`). Replaces the shell's native history search behind that key.
- **M6** — privacy. Regex denylist of commands to skip via config (`[capture] skip = ["^aws .* --secret", …]`). Leading-whitespace skipping is already in M1.
- **Later** — cross-machine sync (shared design with hugin if possible); MCP server exposing history to AI for "what did I run last week that did X?" queries.
