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

Two binaries in the `munin` crate, mirroring hugin's `hugind`/`hugin` split. The daemon is long-running so shell hooks can fire-and-forget to a warm process, and so sync (planned for M6) has somewhere to live.

**Process model.** `munind` is a long-running per-user daemon supervised by `systemd --user` (`Type=notify`). `munin` is a short-lived synchronous CLI that opens the unix socket, writes one JSON line, optionally reads a response, and exits. The CLI does not link tokio. Shell hooks invoke `munin add-start` / `munin add-end` on `preexec` / `precmd`; the daemon hands those off to the storage thread without writing a response, so the shell does not pay a round-trip on every prompt.

**Threading.** `munind` runs a tokio multi-threaded runtime with 2 worker threads (`munin-ipc`) for the unix-socket server, plus a synchronous OS thread (`munin-storage`) that owns the `rusqlite::Connection` and drains an `mpsc<StoreCmd>`. Reads (`list`/`search`/`get`, landing in M2) will open ephemeral connections from inside `spawn_blocking` tasks — WAL mode makes that safe. Writes only ever go through the storage thread.

**Shutdown.** Same shape as hugind. `runtime.block_on(wait_for_shutdown_signal())` blocks main until SIGTERM/SIGINT. Then `STOPPING=1` is sent to systemd, the runtime is dropped (cancelling the IPC server and every per-connection task, which drops all `Sender<StoreCmd>` clones), and the storage thread's `recv` loop exits cleanly. `READY=1` is sent right after the IPC task is spawned.

**Wire protocol.** JSON-lines, modeled on `hugin/src/proto.rs`. Requests carry a `"op"` discriminant, responses a `"kind"`. The protocol is **asymmetric on purpose**: read ops (`ping`, `list`, `search`, `get`, `import`) write a one-line JSON response, but capture ops (`add-start`, `add-end`) write nothing — they exist to be fire-and-forget for shell hooks. The CLI writes the JSON line and exits without reading the response for those. The per-connection loop accepts further requests on the same socket either way. `import` is long-running so its IPC task awaits a `tokio::sync::oneshot` reply that the storage thread sends when it finishes the bulk insert.

**Storage.** DB at `$XDG_DATA_HOME/munin/munin.db` (default `~/.local/share/munin/munin.db`). WAL + foreign keys, schema versioning via `PRAGMA user_version` (current: `1`). Schema-version check runs **before** any pragmas are set so a rejected DB does not leave `-wal`/`-shm` sidecars (same discipline as `hugin/src/storage.rs:ensure_compatible_schema`). Schema apply + version stamp happen inside a single transaction so a crash between them cannot leave a half-stamped DB.

Schema:

- `entries(id, uuid, client_id, cmd, ts_unix_ns, cwd, hostname, session, shell, exit_code, duration_ms, synced_at)` with indexes on `ts_unix_ns`, `(session, ts_unix_ns)`, `cwd`, and a partial index `entries_unsynced_idx ON (synced_at) WHERE synced_at IS NULL`.
- `config(key, value)` — bootstraps `client_id` (a stable per-machine UUIDv4 generated on first start) and will later hold `last_seen` for sync pull.

The `uuid` / `client_id` / `synced_at` columns are reserved now so the M6 sync work does not need a migration. `uuid` is UUIDv7, generated at capture time so rows sort roughly by timestamp without depending on the local clock alone.

**Session matching for add-end.** `add-start` inserts a row with `exit_code = NULL`, `duration_ms = NULL`, and records `(session → (row_id, started_at))` in an in-memory `HashMap` on the `Store`. The matching `add-end` looks up the session, computes `duration_ms`, removes the entry, and `UPDATE`s the row. Orphan `add-end`s (no matching start, e.g. a precmd without a preexec) are dropped at `DEBUG`. The map size is bounded by the number of live shell sessions on the machine.

**Filtering at capture.** Lines whose `cmd` begins with whitespace are dropped silently at the storage layer, matching the standard `HIST_IGNORE_SPACE` / `HISTCONTROL=ignorespace` convention.

**Full-text search (FTS5).** `entries_fts` is a full-content FTS5 virtual table holding one `content` column (the `cmd` text, duplicated on purpose — the smaller external-content variant complicates the insert/delete invariants for marginal disk savings). Inserted alongside `entries` inside the same transaction in both `add_start` and `import_file`. The `entries_ad` `AFTER DELETE` trigger keeps the FTS index in sync when retention (later) or manual deletes remove an `entries` row — the FK CASCADE on related tables doesn't reach virtual tables, so the trigger is load-bearing. Search uses two static SQL variants that differ only in `ORDER BY` (`bm25(entries_fts) ASC, e.id DESC` for `--sort=relevance`, `e.id DESC` for `--sort=recent`), `snippet(entries_fts, 0, '‹', '›', '…', 16)` for highlighting, phrase-wraps non-`--raw` queries (`"foo"` with embedded `"` doubled) so a query containing FTS5 metachars doesn't error. The phrase wrap **trims** before quoting (`"git "` would otherwise match no docs because FTS5 treats the trailing whitespace inside a phrase literally). Empty/whitespace queries short-circuit so we don't ship `""` to FTS5. **No retention sweep yet** — shell history scales to millions of rows comfortably; revisit only if it bites.

**Schema versioning.** `PRAGMA user_version` carries the schema generation (current: `2` = FTS landed). `ensure_compatible_schema` runs **before** any pragmas (so a rejected DB doesn't leave `-wal`/`-shm` sidecars):
- `user_version == 2` + `entries_fts` present → OK.
- `user_version == 2` + `entries_fts` missing → refuse; rebuilding it silently would hide historical rows from search.
- `user_version == 0` and no `entries` table → fresh DB; apply schema + version stamp inside one tx, set version.
- `user_version == 1` (pre-FTS) → refuse, name the file, user deletes it. No automatic migration.

**Sync columns are reserved, sync itself is M6.** The sketch: a small self-hosted server stores opaque end-to-end-encrypted blocks keyed by `uuid`. The symmetric key is derived from a passphrase via Argon2id and never leaves the client. `munind` periodically `SELECT * FROM entries WHERE synced_at IS NULL`, encrypts, POSTs, sets `synced_at = now`. Pull is `INSERT OR IGNORE` keyed on `uuid`. Collisions across machines are astronomically unlikely with UUIDv7; on the off chance one happens, prefer the earlier `ts_unix_ns`.

**Shell integration.** `munin init <shell>` prints a hook script to stdout; users wire it up with `eval "$(munin init zsh)"` (or `bash`) in their rc file. Shell-specific glue lives at `munin/src/shells/<name>.sh` and is embedded via `include_str!`. Each hook script exports `MUNIN_SHELL=<name>` so the CLI's `add-start` knows which shell a row came from — `$SHELL` is the *login* shell and doesn't change when you nest bash inside zsh, so it's the wrong signal.

- **zsh** uses `add-zsh-hook preexec/precmd`; preexec's `$1` is the full command line. Backgrounded with `&!` (background-and-disown) so the shell doesn't wait on the CLI's fork+exec.
- **bash** has no native preexec. The hook installs a `DEBUG` trap plus a `PROMPT_COMMAND` precmd that arms a `_munin_pending` flag at the end of each prompt cycle. The first `DEBUG` of the next cycle consumes the flag and records the command; subsequent `DEBUG`s within the same command (pipeline segments, PROMPT_COMMAND machinery, subshells gated by `BASH_SUBSHELL > 0`) skip. The full command line is read from `builtin history 1` rather than `$BASH_COMMAND` (which only carries one pipeline segment per fire). Child is detached with `( cmd & )`.

Critical gotcha — **PS0 won't carry shell-state across the trap.** An earlier draft used `PS0='$(...)'` to set the pending flag, but `$()` runs the function in a subshell and the assignment never reaches the parent. The working version sets the flag in `PROMPT_COMMAND` (which runs in the parent shell) and only consumes it in the `DEBUG` trap.

Known limitation — **bash strips leading whitespace from `history` entries** (independent of `HISTCONTROL`), so the daemon-side whitespace-prefix filter is a no-op on bash. zsh preserves the prefix and the filter works there. Documented inline in `bash.sh`.

Adding fish or nushell is one more script in `munin/src/shells/` + one variant in the `Shell` enum + one arm in `init_script`; the Rust core stays shell-agnostic.

**Interactive TUI.** `munin search -i` opens an fzf-style picker (`ratatui` + `crossterm`) seeded with the typed query. Type → re-runs the search on every keystroke; Up/Down → move; PageUp/PageDown → jump 10; Enter → print the chosen command to stdout (so the shell hook in M5 can splice it into the line buffer); Esc / Ctrl-C / Ctrl-D-on-empty → exit silently; Tab / Ctrl-R → toggle relevance↔recent; Ctrl-U / Ctrl-W → readline-style line/word delete.

The TUI **bypasses IPC and opens SQLite directly** (`Connection::open(default_db_path())`). Reasons: per-keystroke IPC adds latency, and the TUI should keep working when `munind` is down. WAL mode makes the concurrent read safe.

Critical layout invariant — **fzf-style means best-match nearest the prompt.** ratatui's `List` renders top→bottom, so `refresh_results` reverses the search Vec for `Layout::Bottom` (Vec[0] = worst at top of screen, Vec[len-1] = best, just above the prompt). Initial selection is `len-1`. Up/Down keys keep their normal index semantics on the reversed Vec, which translates to the visually-correct direction (Up moves the highlight upward on screen, toward worse matches; Down moves it downward, toward best).

**Config file** at `$XDG_CONFIG_HOME/munin/config.toml`. All keys optional — missing file or missing keys fall through to defaults; bad TOML logs a warning and the defaults are used (we never refuse to open the TUI over a config error). Current schema:

```toml
sort = "relevance"      # "relevance" | "recent" — initial sort mode
limit = 200             # max rows fetched per keystroke
layout = "bottom"       # "bottom" (fzf-style) | "top"
[colors]
selection_fg = "black"
selection_bg = "cyan"
match_fg = "yellow"
prompt_fg = "green"
status_fg = "gray"
```

Colours accept the named ANSI palette (`black`/`red`/.../`gray`/`darkgray`/`light*`). Hex / 24-bit can be added later without breaking existing configs because `serde(deny_unknown_fields)` is **not** set on the colour palette — only the top-level config — so colour additions are forward-compatible. Adding a new shell-script knob (or future TUI option) means another field with a `Default` impl and a docs line here.

**Logging.** `tracing` → stderr, gated by `MUNIN_LOG` (full `tracing-subscriber` EnvFilter syntax). Default level is `info`. One `INFO add-start id=… session=… cmd=…` per captured row; `DEBUG` for whitespace-skips and orphan `add-end`s.

**Critical invariants worth preserving.** Several patterns are load-bearing and shared with hugin:
- The schema-version gate runs **before** pragmas, so a rejected DB does not scatter `-wal`/`-shm` sidecars.
- Schema apply + `user_version` stamp must commit in the same transaction (otherwise a crash mid-startup leaves a v0 DB that the next start refuses as pre-versioning).
- The `bind_clean` stale-socket probe in `ipc::serve`: if the socket exists and accepts a connection, refuse to start; otherwise unlink the dead socket. Same pattern as `hugin/src/ipc.rs`.
- All future `list`/`search` endpoints must clamp `limit` to a `MAX_LIMIT` constant before binding it into SQL — SQLite treats `usize::MAX as i64 == -1` as "no limit".

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

Numbered milestones, same convention. Daemon-first design (decided during M0 planning; supersedes an earlier no-daemon sketch).

- **M0** (done) — `munind` skeleton. `munind` + `munin` binaries, tokio multi-thread runtime, unix socket at `$XDG_RUNTIME_DIR/munin.sock` with `bind_clean` stale-socket probe, JSON-lines protocol, `ping` + `add-start` + `add-end` ops, storage thread on `mpsc<StoreCmd>`, schema v1 with `uuid` / `client_id` / `synced_at` columns reserved for sync, `client_id` generated on first start, `MUNIN_LOG` env-filter tracing, sd-notify `READY`/`STOPPING`, graceful SIGTERM/SIGINT shutdown, systemd user unit at [`dist/munind.service`](dist/munind.service). Whitespace-prefixed commands are dropped at the storage layer.
- **M1** (done) — Shell hooks. `munin init zsh` / `munin init bash` print hook scripts embedded via `include_str!` from `munin/src/shells/{zsh,bash}.sh`. zsh uses `add-zsh-hook preexec/precmd` with `&!` (background-and-disown). bash uses a `DEBUG` trap + `PROMPT_COMMAND`-armed flag (`_munin_pending`) to record only the first `DEBUG` of each prompt cycle, reads the full command line (including pipelines) from `builtin history 1`, and uses `( cmd & )` to detach the child from job control. Hooks export `MUNIN_SHELL=zsh|bash` so the CLI can record which shell a command came from (the user's login `$SHELL` doesn't change for nested bash). Verified end-to-end with `script(1)`-driven pty sessions: pipelines stored as one line, exit codes preserved, durations accurate, distinct sessions, whitespace-prefix skip works on zsh. **Known bash limitation:** bash strips leading whitespace from `history` entries, so the daemon-side whitespace filter is a no-op for bash; documented inline in `bash.sh`.
- **M2** (done) — Read CLI: `munin list` (alias `ls`), `munin search <query>` (alias `s`, LIKE-based with `%`/`_`/`\` escaped via `ESCAPE '\'`), `munin get <id>` (alias `info`), `munin import <path> <zsh|bash>`. Filters on list/search: `--limit`, `--cwd`, `--session`, `--shell`, `--since`, `--until` (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` in local TZ). Reads open ephemeral `Connection`s inside `spawn_blocking`; writes (`import`) route through the storage thread via a `tokio::sync::oneshot` reply channel so the IPC task awaits completion. History parsers in `storage.rs`: zsh extended (`: ts:dur;cmd`) with backslash-continuation for multi-line entries, bash `HISTTIMEFORMAT` (`#<unix-ts>\n<cmd>`); plain lines get synthesised sequential timestamps so file order is preserved. Limit clamped to `MAX_LIMIT = 10_000`. Empty queries short-circuit to no results.
- **M3** (done) — FTS5 search. Full-content `entries_fts` table on `cmd`, `entries_ad` `AFTER DELETE` trigger, `--sort=relevance|recent` (default relevance via `bm25(entries_fts) ASC`), `--raw` toggles FTS5 operator passthrough (AND/OR/NEAR/prefix*), `snippet(...)` excerpts with `‹›` highlight markers, schema bumped to `user_version = 2`. Schema-version gate refuses pre-FTS (v1) DBs and v2 DBs whose `entries_fts` was dropped out-of-band — both name the file and tell the user to delete it. Verified bm25 ranking, recent ordering, phrase vs raw queries, prefix wildcards, snippet markers, and no `-wal`/`-shm` sidecars leak on rejection.
- **M4** (done) — Interactive TUI. `munin search -i` opens an fzf-style picker (`ratatui` + `crossterm`) seeded with the typed query. Type → live search; Up/Down/PageUp/PageDown → navigate; Enter → print the chosen command to stdout; Esc/Ctrl-C → exit silently; Tab/Ctrl-R → toggle relevance↔recent; Ctrl-U/Ctrl-W → readline-style edits. Reads SQLite directly (bypasses the daemon) so the TUI works even when `munind` is down. Config file at `$XDG_CONFIG_HOME/munin/config.toml` (sort/limit/layout + named-ANSI colours) — all optional, bad config warns and falls through to defaults. Schema details in the architecture section above. Also fixes a latent M3 search bug: phrase queries are now trimmed before quoting so `"git "` matches the same docs as `"git"`.
- **M5** (next) — shell binding. `munin init <shell>` output gains a Ctrl-R binding that runs `munin search -i` and splices the chosen command into the line buffer (zsh: `BUFFER=…; zle reset-prompt`; bash: `READLINE_LINE=…; READLINE_POINT=…`). Replaces the shell's native history search behind that key.
- **M6** — Sync. Self-hosted server, end-to-end encryption (Argon2id-derived symmetric key), push-unsynced + pull-since loop in `munind`. Schema is already ready for it (see "Sync columns" in the architecture section above).
- **Later** — MCP server exposing history to AI for "what did I run last week that did X?" queries.
