# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project vision

A Rust workspace of personal-computing tools for a sway/wlroots Linux desktop, plus two shared library crates they all build on. Norse names throughout; Jef Raskin (*The Humane Interface*) + Don Norman lineage across the interactive pickers.

**Tools** (one binary family each):

- **hugin** — Wayland clipboard manager for sway/wlroots compositors. Watches the clipboard via `wlr-data-control-unstable-v1` and persists every selection change. fzf-style search picker (`hugin search -i`). Eventual goals: expose clipboard history to AI via an MCP server, and synchronize between machines.
- **munin** — atuin-like shell history search with an fzf-style UI, fully customisable by the user. Daemon (`munind`) + CLI (`munin`); captures write directly to SQLite from shell hooks.
- **mimir** — compact interactive status bar for sway (an i3status replacement). Single foreground process speaking the swaybar/i3bar JSON protocol; reads `/proc` & `/sys` via `nom`. No SQLite, no TUI picker.
- **sleipnir** — modeless frecency navigator (`Ctrl-T`): one picker over directories *and* files you've touched (dirs from a chpwd hook, files mined from munin's history).
- **valkyrie** — modeless, humane process finder/handler (`Alt-P`): fuzzy-find processes and signal them (SIGTERM / hold-to-SIGKILL / STOP-CONT / any signal). No daemon, no storage.
- **heimdall** — rootless, presence-first LAN device finder: concurrent ARP-sweep + mDNS + SSDP, live picker. No persistent storage — a purely live, in-memory presence view (who's up *now*); relaunch rediscovers from scratch. Background tokio engine → sync ratatui TUI over a channel.

**Shared library crates** (see "Shared crates" below):

- **levelup-core** — leaf helpers with no TUI deps: `init_tracing`, `sanitize_display`, `fuzzy` (nucleo matcher/pattern/highlight), `sqlite` (schema-version gate), `xdg` (base-dir paths), `completions` (shared `clap_complete` subcommand plumbing).
- **levelup-tui** — picker building blocks: `config` (ANSI palette / layout enums + TOML loader), `editing` (readline cursor/kill helpers), `highlight` (`‹›`-marker → ratatui spans), `terminal` (`/dev/tty` alternate-screen setup/teardown).

Tools use `clap`, `thiserror`, `anyhow`, and `tracing` from `[workspace.dependencies]`.

## Workspace shape

Edition `2024`, resolver `"3"`. The workspace `Cargo.toml` at the repo root pins shared dependency versions; members inherit them with `.workspace = true`. Members: `levelup-core`, `levelup-tui`, `hugin`, `munin`, `mimir`, `sleipnir`, `valkyrie`, `heimdall`. The repo root is the git repository; the authoritative lockfile is `./Cargo.lock`.

Every binary embeds its git commit in `--version` via a per-crate `build.rs` (`cargo:rustc-env=GIT_COMMIT` → `VERSION = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_COMMIT"), ")")`).

## Shared crates (levelup-core / levelup-tui)

Extracted from five near-identical picker copies (the agreed trigger; see the roadmap notes). The extraction is a **toolkit, not a framework** — shared *leaf helpers and types*, while each tool keeps its own `State`, event loop, and render code. This is deliberate: valkyrie's process tree, heimdall's flexible `Table` + detail strip, and the modal choosers don't fit one generic `Picker` abstraction.

- **levelup-core** (`fuzzy`, `sqlite`, `xdg`, `completions`, `init_tracing`, `sanitize_display`) — no ratatui/crossterm deps, so non-TUI crates (mimir) can use it too. Pulls in `clap` + `clap_complete` for `completions`.
  - `fuzzy::{matcher, pattern, highlight_indices}` — nucleo with fzf defaults (`CaseMatching::Smart` + `Normalization::Smart`); `highlight_indices` wraps matched codepoint runs in `‹…›`.
  - `sqlite::ensure_compatible_schema(conn, path, version, sentinel)` + `table_exists` — the version-gate-before-pragmas discipline, parameterised by schema version and the table whose presence means "not a fresh DB".
  - `xdg::{data_file, config_file, cache_file, state_file, runtime_file, …}` — return types chosen to match existing call sites (`config_file` → `Option`, `data_file`/`state_file` → `Result`). `state_file` (XDG_STATE_HOME → `~/.local/state`) is for logs and other persistent-but-disposable data — munin's auto-started daemon logs there.
  - `completions::{print, detect_shell}` + re-exported `Shell` — the shared `completions [SHELL]` subcommand plumbing. Every CLI declares `Completions { shell: Option<Shell> }` (alias `comp`) and calls `completions::print(&mut Cli::command(), bin, shell)`; `SHELL` defaults to `$SHELL`'s basename (`detect_shell`), erroring if undetectable. So all six CLIs behave identically. hugin's *daemon* (`hugind`) keeps its older `--generate-completions` flag (a daemon arg-struct, no subcommands) — the one intentional exception.
- **levelup-tui** (`config`, `editing`, `highlight`, `terminal`) — depends on levelup-core + ratatui/crossterm.
  - `config::{ColorName, Layout, load_or_default}` — the named-ANSI palette, layout enum, and the generic `load_or_default::<T>(Option<PathBuf>)` loader (warn-and-default on bad TOML). Each tool's `config.rs` is now a thin shim: `pub use` the enums, declare its own `Config`/`Colors` field set, and call the generic loader.
  - `editing::{prev_offset, next_offset, delete_back, delete_forward, kill_to_end, kill_line, delete_word}` — return `bool` (changed?), so each tool maps that onto its own refresh mechanism (e.g. munin/hugin's `KeyOutcome`, valkyrie's direct `refresh_results`).
  - `highlight::highlight_snippet(s, match_fg, base)` — parses the `‹›` markers into ratatui spans (calls `levelup_core::sanitize_display` first).
  - `terminal::{setup, restore}` — `setup` opens `/dev/tty` (the load-bearing not-stdout trick) and returns a `File`-backed terminal; all six pickers (hugin included) use it, so the alternate screen never lands on stdout and piping output stays clean. `restore` is generic over the backend writer.

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
- Schema: `entries(id, ts_unix_ns, selection, hash, size_bytes, text)` + `mime_parts(entry_id, mime, blob)` with `ON DELETE CASCADE`. Indexes on `hash`, `ts_unix_ns`, `(selection, id)`. The `text` column holds the entry's indexable plaintext (NULL for image/binary-only entries) — it's what fuzzy search matches and what the preview/snippet are sliced from.
- **Dedup** is blake3 over canonical `(mime, blob)` parts sorted by MIME. A new capture is compared to the most recent entry *for the same selection*; identical → skipped, logged at `DEBUG`.
- **Retention** (`Store::maybe_retain`): at most once per hour. Deletes by age (default 90 days) and trims to most-recent N (default 10 000). Defaults in `RetentionConfig`.
- Storage is content-addressable on purpose, to keep the future cross-machine sync layer tractable.

**Fuzzy search (nucleo-matcher).** Same matcher and shape as munin (FTS5 was tried and dropped — see roadmap M5/M6).
- At capture, `pick_indexable_text` chooses the plaintext stored in `entries.text`: a tiered single-pass picker `text/plain` (UTF-8) > other `text/*` (UTF-8) > any text-MIME via lossy UTF-8 decode. The lossy tier exists so legacy X11 atoms like `STRING` (Latin-1) still index — non-UTF-8 bytes become U+FFFD. Image/binary-only entries store `text = NULL` and are unsearchable (they only surface under an empty query, rendered with a `[mime]` label).
- `storage::search` pulls up to `MAX_LIMIT` recent rows (filters applied at SQL) via `SEARCH_POOL_SQL`, then scores each entry's `text` with `nucleo_matcher::pattern::Pattern::indices` (`CaseMatching::Smart` + `Normalization::Smart`, fzf defaults). Each haystack is capped to `MATCH_PREFIX` (4096) chars — clipboard payloads can be megabytes and a clipping's distinguishing tokens live up front; this keeps per-keystroke scoring bounded. `SearchSort::Relevance` = nucleo score desc, `id` desc tiebreak; `Recent` = `id` desc post-filter.
- `EntryMeta.snippet` for `search` is built by `highlight_indices(&str, &[u32])` from the matched codepoint indices, wrapping matched chars in `‹…›` (multi-byte-safe via `chars().enumerate()`). For `list`/`get` it's `substr(text, 1, 200)`. Same field, two contents: a leading excerpt or a highlight-marked one. The CLI's `print_table` balances unclosed `‹` markers after truncation; the TUI's `highlight_snippet` consumes the same markers.
- `list`/`search` clamp `limit` to `MAX_LIMIT` (10 000) before binding it. SQLite treats a negative LIMIT as unlimited and `usize::MAX as i64` wraps to -1 — without the clamp any IPC peer could materialise the whole table.
- `search` with an empty/whitespace query falls through to `list` (most-recent N, leading-excerpt snippets, no scoring).

**Schema versioning.** `PRAGMA user_version` carries the schema generation (current: `1` — rolled back from the brief FTS `2`, mirroring munin). `Store::open` calls `ensure_compatible_schema` *before* setting any pragmas (so a rejected DB doesn't leave `-wal`/`-shm` sidecars), then applies `SCHEMA_SQL` + the version stamp inside a single transaction (so a crash between them can't strand a half-stamped DB the next start would reject). Same discipline as `munin/src/storage.rs`:
- `user_version == DB_VERSION` → OK.
- `user_version == 0` and no `entries` table → fresh DB; apply schema, set version.
- Anything else (a pre-versioning DB with an `entries` table, or the brief FTS `2`) → refuse, name the file. No automatic migration — dev-stage, so the user deletes the file and the daemon recreates it.

**Two selections, two streams.** `Selection::Regular` = the CLIPBOARD selection (Ctrl-C / Ctrl-V), always watched. `Selection::Primary` = the PRIMARY selection (auto-populated by mouse text selection, pasted via middle-click), **off by default** because mouse-drag selection in many apps emits a steady stream of intermediate MIMEs that crowds the history. Enable with `hugind --primary`. Many apps populate both on Ctrl-C, so with `--primary` you'll often see two entries per copy.

**Logging.** `tracing` → stderr. `HUGIN_LOG` controls filter (full `tracing-subscriber` EnvFilter syntax). One `INFO stored …` line per persisted capture; `DEBUG` for dedup hits and selection-cleared events.

## munin architecture

Two binaries in the `munin` crate, mirroring hugin's `hugind`/`hugin` split. The daemon is long-running so bulk `import` and sync (planned for M6) have somewhere to live. **Captures do not go through the daemon** — the shell hooks write directly to SQLite (see Process model below), so history is recorded whether or not `munind` is running.

**Process model.** `munind` is a long-running per-user daemon; `munin` is a short-lived synchronous CLI. **The CLI owns the daemon's lifecycle — there is no systemd unit** (it was removed; the README documents migrating off it). The daemon's job is bulk `import` now (and cross-machine sync later); `munin daemon ping` is a liveness probe against it. **The daemon is not on the capture hot path** — captures (`add-start`/`add-end` from shell hooks) **write directly to SQLite from the CLI** via `Store::open(default_db_path())` + `Store::add_start`/`add_end`, so capture keeps working when `munind` is down — atuin's default model (its daemon is opt-in for batching/sync too). Reads (`list` / `search` / `get`) likewise open the SQLite file directly via `Connection::open(default_db_path())` and call the standalone `storage::{list,search,get}` functions; they don't go through IPC. The TUI (`munin search -i`) follows the same pattern (`munin/src/tui.rs:59`). WAL mode + `PRAGMA busy_timeout` make the CLI's concurrent reads/writes safe alongside the daemon's `import`. The CLI does not link tokio for any subcommand.

**Daemon lifecycle (CLI-managed, no systemd).** `munin/src/daemon.rs` is the client-side lifecycle module. The daemon-routed subcommand `import` calls `daemon::ensure_running(socket, db)` before connecting, which **lazily auto-starts** `munind` if the socket is dead. The auto-start is **race-free**: try-connect → acquire an exclusive `flock` on `$XDG_RUNTIME_DIR/munin.spawn.lock` → try-connect again (double-checked, another process may have won) → `spawn_detached` (locate `munind` next to the running `munin` via `current_exe()`, else PATH; `pre_exec(setsid)` + stdio redirected to the log so it outlives the shell; pass through `--socket`/`--db`) → poll the socket until it accepts (~20 ms cadence, ~2 s budget). The `flock` serializes a thundering herd of spawners; the daemon's existing `bind_clean` (`ipc.rs`) is the backstop if two ever race past it. Captures/reads/TUI **never** call `ensure_running` — the daemon-free guarantee holds. Explicit control: `munin daemon {start,stop,status,restart,ping}` (`run_daemon` in `bin/munin.rs`). **`daemon ping` is a pure probe** — `daemon::ping` connects and sends `Ping` but deliberately does *not* auto-start (a probe that starts what it probes is surprising); prints `ok` or `not running` and `exit(1)` when down. Self-spawned daemon logs go to `$XDG_STATE_HOME/munin/munind.log` (append; `levelup_core::xdg::state_file`).

**Shell completions.** `munin completions [SHELL]` (alias `comp`) prints a `clap_complete` script to stdout via the shared `levelup_core::completions::print` helper (`SHELL` defaults to `$SHELL`). Handled in the prefix `match` before any daemon work. This is the same helper every CLI uses — see the `completions` bullet under "Shared crates".

**Threading.** `munind` runs a tokio multi-threaded runtime with 2 worker threads (`munin-ipc`) for the unix-socket server, plus a synchronous OS thread (`munin-storage`) that owns the `rusqlite::Connection` and drains an `mpsc<StoreCmd>`. The storage thread now handles only `import` (and future sync) — captures and reads don't go through the daemon at all; the CLI opens its own ephemeral connection per invocation (a `Store` for captures, a bare `Connection` for reads). WAL + `busy_timeout` keep these concurrent writers safe.

**Shutdown.** Same shape as hugind. `runtime.block_on(wait_for_shutdown_signal())` blocks main until SIGTERM/SIGINT. Then `STOPPING=1` is sent to systemd (a no-op when not run under systemd, which is now the normal case — `NOTIFY_SOCKET` unset), the runtime is dropped (cancelling the IPC server and every per-connection task, which drops all `Sender<StoreCmd>` clones), and the storage thread's `recv` loop exits cleanly. `READY=1` is sent right after the IPC task is spawned. The `Shutdown` IPC op drives this exact path by `raise`-ing SIGTERM on the daemon itself, so `munin daemon stop` and a manual SIGTERM are identical.

**Wire protocol.** JSON-lines, modeled on `hugin/src/proto.rs`. Requests carry a `"op"` discriminant, responses a `"kind"`. Four ops live on the wire: `ping` (writes `Ok`; backs `munin daemon ping`), `import` (writes `Imported{inserted}` or `Error{message}` after the bulk-insert finishes via a `tokio::sync::oneshot` reply from the storage thread), `status` (writes `Status{pid,version,started_unix_ns,db_path,socket_path}` from the `DaemonInfo` threaded into `ipc::serve`; backs `munin daemon status`, grows sync fields in M6), and `shutdown` (writes `Ok` then `raise(SIGTERM)`; backs `munin daemon stop`). The `add-start` / `add-end` ops were removed once captures moved to direct-SQLite in the CLI (same move the `list` / `search` / `get` read ops made earlier) — easy to re-add when an MCP server (or any remote consumer) needs them. The dispatcher will call the same `storage::*` functions the CLI now uses.

**Storage.** DB at `$XDG_DATA_HOME/munin/munin.db` (default `~/.local/share/munin/munin.db`). WAL + foreign keys, schema versioning via `PRAGMA user_version` (current: `1`). Schema-version check runs **before** any pragmas are set so a rejected DB does not leave `-wal`/`-shm` sidecars (same discipline as `hugin/src/storage.rs:ensure_compatible_schema`). Schema apply + version stamp happen inside a single transaction so a crash between them cannot leave a half-stamped DB.

Schema:

- `entries(id, uuid, client_id, cmd, ts_unix_ns, cwd, hostname, session, shell, exit_code, duration_ms, synced_at)` with indexes on `ts_unix_ns`, `(session, ts_unix_ns)`, `cwd`, and a partial index `entries_unsynced_idx ON (synced_at) WHERE synced_at IS NULL`.
- `config(key, value)` — bootstraps `client_id` (a stable per-machine UUIDv4 generated on first start) and will later hold `last_seen` for sync pull.

The `uuid` / `client_id` / `synced_at` columns are reserved now so the M6 sync work does not need a migration. `uuid` is UUIDv7, generated at capture time so rows sort roughly by timestamp without depending on the local clock alone.

**Session matching for add-end (stateless, SQL).** Because `add-start` (preexec) and `add-end` (precmd) now run as *separate* short-lived CLI processes writing directly to SQLite, the match can't live in process memory — it's recovered from the DB. `add-start` inserts a row with `exit_code = NULL`, `duration_ms = NULL`. `add-end` finds the newest still-open row for the session (`SELECT id, ts_unix_ns FROM entries WHERE session = ? AND exit_code IS NULL ORDER BY id DESC LIMIT 1`), computes `duration_ms` from that row's own `ts_unix_ns`, and `UPDATE`s it. Interactive shells run commands serially, so "newest open row for this session" is unambiguous (at most one in flight). Orphan `add-end`s (no matching start, e.g. a precmd without a preexec) are dropped at `DEBUG`. (The earlier design kept an in-memory `HashMap<session,(row_id,started_at)>` on the daemon's `Store`; that's gone with the daemon-free capture path.)

**Filtering at capture.** Lines whose `cmd` begins with whitespace are dropped silently at the storage layer, matching the standard `HIST_IGNORE_SPACE` / `HISTCONTROL=ignorespace` convention.

**Fuzzy search (nucleo-matcher).** `storage::search` pulls up to `MAX_LIMIT` (10 000) recent rows via `LIST_SQL` (filters still apply at SQL), then scores each `cmd` against the query with `nucleo_matcher::pattern::Pattern::indices` using `CaseMatching::Smart` + `Normalization::Smart` (same defaults as fzf). Non-matches are dropped. `SearchSort::Relevance` sorts by nucleo score desc with `id` desc as tiebreak; `SearchSort::Recent` sorts by `id` desc post-filter. `EntryMeta.snippet` is built from the matched codepoint indices and wraps matched chars in `‹…›` — the same markers the TUI's `highlight_snippet` consumes — via `highlight_indices(&str, &[u32])`, which iterates `chars().enumerate()` so multi-byte chars don't shift the markers. Empty/whitespace queries fall through to `list` (most-recent N, no scoring, no snippets). Atuin uses nucleo too — they forked it into their workspace as `atuin-nucleo`/`atuin-nucleo-matcher`, original author Pascal Kuthe, same as upstream. **No retention sweep yet** — shell history scales to millions of rows comfortably; revisit only if it bites.

**Schema versioning.** `PRAGMA user_version` carries the schema generation (current: `1`). `ensure_compatible_schema` runs **before** any pragmas (so a rejected DB doesn't leave `-wal`/`-shm` sidecars):
- `user_version == 1` → OK.
- `user_version == 0` and no `entries` table → fresh DB; apply schema + version stamp inside one tx, set version.
- Anything else (including the brief v2 that shipped FTS5 before nucleo replaced it) → refuse, name the file, user deletes it. No automatic migration — this is dev-stage.

**Sync columns are reserved, sync itself is M6.** The sketch: a small self-hosted server stores opaque end-to-end-encrypted blocks keyed by `uuid`. The symmetric key is derived from a passphrase via Argon2id and never leaves the client. `munind` periodically `SELECT * FROM entries WHERE synced_at IS NULL`, encrypts, POSTs, sets `synced_at = now`. Pull is `INSERT OR IGNORE` keyed on `uuid`. Collisions across machines are astronomically unlikely with UUIDv7; on the off chance one happens, prefer the earlier `ts_unix_ns`.

**Shell integration.** `munin init <shell>` prints a hook script to stdout; users wire it up with `eval "$(munin init zsh)"` (or `bash`) in their rc file. Shell-specific glue lives at `munin/src/shells/<name>.sh` and is embedded via `include_str!`. Each hook script exports `MUNIN_SHELL=<name>` so the CLI's `add-start` knows which shell a row came from — `$SHELL` is the *login* shell and doesn't change when you nest bash inside zsh, so it's the wrong signal.

- **zsh** uses `add-zsh-hook preexec/precmd`; preexec's `$1` is the full command line.
- **bash** has no native preexec. The hook installs a `DEBUG` trap plus a `PROMPT_COMMAND` precmd that arms a `_munin_pending` flag at the end of each prompt cycle. The first `DEBUG` of the next cycle consumes the flag and records the command; subsequent `DEBUG`s within the same command (pipeline segments, PROMPT_COMMAND machinery, subshells gated by `BASH_SUBSHELL > 0`) skip. The full command line is read from `builtin history 1` rather than `$BASH_COMMAND` (which only carries one pipeline segment per fire).

**Critical gotcha — capture must run synchronously.** Both `add-start` (preexec) and `add-end` (precmd) run in the *foreground* (no `&!` on zsh, no `( cmd & )` on bash). They used to be backgrounded "so the shell doesn't wait on the CLI", but that's incompatible with the daemon-free direct-write capture: backgrounded captures reorder, and since `add-end` pairs to the session's open row by SQL (`exit_code IS NULL ORDER BY id DESC`), a reordered `add-end` closes the *wrong* command — exit codes get attributed to neighbouring commands — and a process can be torn down on shell exit, dropping a row. Foreground keeps the two calls ordered; within one shell commands are serial, so there is exactly one open row per session at precmd time. Cost is ~5 ms per call (process spawn + WAL write), measured imperceptible. A command that ends the shell (`exit`, `logout`) gets a preexec but no precmd, so it stays as an open row with no exit code — inherent, same as atuin.

Critical gotcha — **PS0 won't carry shell-state across the trap.** An earlier draft used `PS0='$(...)'` to set the pending flag, but `$()` runs the function in a subshell and the assignment never reaches the parent. The working version sets the flag in `PROMPT_COMMAND` (which runs in the parent shell) and only consumes it in the `DEBUG` trap.

Known limitation — **bash strips leading whitespace from `history` entries** (independent of `HISTCONTROL`), so the daemon-side whitespace-prefix filter is a no-op on bash. zsh preserves the prefix and the filter works there. Documented inline in `bash.sh`.

Adding fish or nushell is one more script in `munin/src/shells/` + one variant in the `Shell` enum + one arm in `init_script`; the Rust core stays shell-agnostic.

**Interactive TUI.** `munin search -i` opens an fzf-style picker (`ratatui` + `crossterm`) seeded with the typed query. Two-action selection (atuin-style): **Enter** runs the chosen command immediately (exit 0), **Tab** drops it on the command line for editing without running (exit 2), **Esc / Ctrl-C** cancels silently (exit 1). The shell hook in M5 reads the exit code to decide which path to take.

Prompt editing supports the standard Emacs / readline set:
- char movement: Left / Right / Ctrl-B / Ctrl-F
- line ends: Home / Ctrl-A, End / Ctrl-E
- delete: Backspace / **Ctrl-H** (some terminals send 0x08 for the Backspace key); Ctrl-D deletes forward (or cancels on empty query); Ctrl-K kills to end of line
- word/line kill: Ctrl-W (word back from cursor), Ctrl-U (whole line)
- list nav: Up / Down, PageUp / PageDown
- sort toggle: Ctrl-R (relevance ↔ recent)

The cursor is a byte offset into `query`, maintained on a UTF-8 char boundary by every mutating handler (`prev_char_offset` / `next_char_offset` step by codepoint, not bytes).

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
status_fg = "darkgray"
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
- **M5** (done) — search. First shipped on FTS5 (`bm25`, `--raw` passthrough, schema v2), then replaced with in-process `nucleo-matcher` (same trajectory as munin — token matching felt wrong in a picker, `gcm` didn't hit `git commit -m`). `hugin search <query>` (alias `s`) with `--sort=relevance|recent` (default relevance), `--limit`, `--selection`. Indexable text moved from the `entries_fts` virtual table to an inline `entries.text` column; the FTS table + `entries_ad` trigger + per-write FTS insert are gone. `--raw` removed. Schema rolled back to v1 (no migration — dev-stage; the on-disk v2 DB is refused and the user deletes it).
- **M6** (done) — interactive picker. `hugin search -i [QUERY]` opens an fzf-style `ratatui`/`crossterm` TUI, mirroring munin's `search -i` (same readline/nav keymap, fzf-style bottom layout, `‹›` highlights, config file). Clipboard-specific differences: **Enter** copies the whole entry to the clipboard (IPC `Copy`), **Tab** prints the entry's content to stdout (exit 0; pipe-friendly), **Ctrl-O** opens a MIME chooser then copies one MIME (`Copy { mime: Some(..) }`), **Ctrl-X** deletes the entry after a `y`/`n` confirm (new IPC `Delete`), **Esc/Ctrl-C/Ctrl-G** cancel. Reads (search + a right-side **preview pane** of the selected entry's content/metadata) go direct-to-SQLite so the picker works with `hugind` down; copy/delete round-trip through `client::Client` and report failures in the status line. Config at `$XDG_CONFIG_HOME/hugin/config.toml` (sort/limit/layout/colours + a `preview` toggle), ported from munin's `config.rs`. `Delete` opens its own FK-on, busy-timeout connection in `spawn_blocking` rather than routing through the storage thread (reads already open their own connections; WAL + busy_timeout make the concurrent write safe).
- **Later** — MCP server exposing history to AI; cross-machine sync.

### Munin

Numbered milestones, same convention. Daemon-first design (decided during M0 planning; supersedes an earlier no-daemon sketch).

- **M0** (done) — `munind` skeleton. `munind` + `munin` binaries, tokio multi-thread runtime, unix socket at `$XDG_RUNTIME_DIR/munin.sock` with `bind_clean` stale-socket probe, JSON-lines protocol, `ping` + `add-start` + `add-end` ops, storage thread on `mpsc<StoreCmd>`, schema v1 with `uuid` / `client_id` / `synced_at` columns reserved for sync, `client_id` generated on first start, `MUNIN_LOG` env-filter tracing, sd-notify `READY`/`STOPPING`, graceful SIGTERM/SIGINT shutdown, systemd user unit at [`dist/munind.service`](dist/munind.service). Whitespace-prefixed commands are dropped at the storage layer.
- **M1** (done) — Shell hooks. `munin init zsh` / `munin init bash` print hook scripts embedded via `include_str!` from `munin/src/shells/{zsh,bash}.sh`. zsh uses `add-zsh-hook preexec/precmd`. bash uses a `DEBUG` trap + `PROMPT_COMMAND`-armed flag (`_munin_pending`) to record only the first `DEBUG` of each prompt cycle, reads the full command line (including pipelines) from `builtin history 1`. Both run capture **synchronously** (see the synchronous-capture gotcha above) — originally backgrounded (`&!` / `( cmd & )`), changed when captures moved to daemon-free direct-write because backgrounding reordered start/end and scrambled exit codes. Hooks export `MUNIN_SHELL=zsh|bash` so the CLI can record which shell a command came from (the user's login `$SHELL` doesn't change for nested bash). Verified end-to-end with `script(1)`-driven pty sessions: pipelines stored as one line, exit codes preserved, durations accurate, distinct sessions, whitespace-prefix skip works on zsh. **Known bash limitation:** bash strips leading whitespace from `history` entries, so the daemon-side whitespace filter is a no-op for bash; documented inline in `bash.sh`.
- **M2** (done) — Read CLI: `munin list` (alias `ls`), `munin search <query>` (alias `s`), `munin get <id>` (alias `info`), `munin import {zsh|bash|atuin} [PATH]`. Filters on list/search: `--limit`, `--cwd`, `--session`, `--shell`, `--since`, `--until` (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` in local TZ). Reads open ephemeral `Connection`s inside `spawn_blocking`; writes (`import`) route through the storage thread via a `tokio::sync::oneshot` reply channel so the IPC task awaits completion. **Import sources:** zsh extended (`: ts:dur;cmd`) with backslash-continuation, bash `HISTTIMEFORMAT` (`#<unix-ts>\n<cmd>`), and atuin `history.db` via read-only SQLite — atuin's row id (a UUIDv7) is preserved as munin's `uuid`, so `INSERT OR IGNORE` makes re-imports idempotent and the imported rows are tagged `shell="atuin"` for filtering. Plain shell-history lines without timestamps get synthesised sequential timestamps so file order is preserved. Limit clamped to `MAX_LIMIT = 10_000`. Empty queries short-circuit to no results.
- **M3** (done) — Fuzzy search. First shipped on FTS5 (`bm25` ranking, `--raw` operator passthrough, schema v2); replaced with in-process `nucleo-matcher` once the TUI made token-based whole-word matching feel wrong (typing `gcm` did not match `git commit -m`). nucleo gives fzf-style subsequence scoring out of the box, the snippet markers stay the same `‹›` so the TUI's renderer is unchanged, and the `entries_fts` virtual table + `entries_ad` trigger + per-write FTS insert are all gone. `--sort=relevance|recent` keeps working (relevance = nucleo score desc, recent = `id` desc post-filter); `--raw` is gone. Schema rolled back to v1 (no migration code because dev-stage — the v2 DB on disk gets refused and the user deletes it). Atuin uses the same matcher (forked as `atuin-nucleo`).
- **M4** (done) — Interactive TUI. `munin search -i` opens an fzf-style picker (`ratatui` + `crossterm`) seeded with the typed query. Two-action selection (atuin-style): **Enter** = run immediately (exit 0); **Tab** = drop on the command line for editing (exit 2); **Esc/Ctrl-C** = cancel (exit 1). The shell hook in M5 reads the exit code. Full Emacs/readline editing in the prompt: Ctrl-A/E (line ends), Ctrl-B/F + Left/Right (cursor), Ctrl-P/Ctrl-N (list nav, alias of Up/Down), Backspace/Ctrl-H (delete back), Ctrl-D (delete forward / cancel on empty), Ctrl-K (kill to end), Ctrl-W (kill word back), Ctrl-U (kill line), Ctrl-R (toggle relevance↔recent). UTF-8-safe via `prev_char_offset`/`next_char_offset`. Reads SQLite directly (bypasses the daemon) so the TUI works even when `munind` is down. Config file at `$XDG_CONFIG_HOME/munin/config.toml` (sort/limit/layout + named-ANSI colours) — all optional, bad config warns and falls through to defaults. Also adds the atuin importer (covered in M2 above). Search backend swapped from FTS5 to nucleo during this milestone (see M3 note).
- **M5** (done) — Shell binding. `munin init <shell>` output now includes a `_munin_search` widget bound to Ctrl-R that runs `munin search -i -- "$BUFFER"` and consumes the TUI's exit-code contract. zsh honours the three outcomes (0 → `BUFFER=…; zle accept-line`; 2 → `BUFFER=…; zle reset-prompt`; 1 → `zle reset-prompt`). bash uses `bind -x` + `READLINE_LINE`/`READLINE_POINT`; known limitation — `bind -x` cannot trigger Enter from inside the bound function, so exit 0 and exit 2 both land the command on the prompt and the user hits Enter to run. Reads-bypass-daemon (also shipped in this milestone) means Ctrl-R works with `munind` down.

**Critical gotcha — the picker renders to `/dev/tty`, not stdout.** The shell hook captures the chosen command with `chosen=$(munin search -i …)`, which captures the picker's **stdout**. So the TUI must NOT draw to stdout — the shared `levelup_tui::terminal::setup` opens `/dev/tty` read+write and points the `CrosstermBackend` at *that* (the fzf/atuin approach); only the final chosen command is `println!`'d to stdout (`bin/munin.rs::run_tui`). crossterm reads key events from `/dev/tty` itself, so input is unaffected. If the backend is ever switched back to `io::stdout()`, the alternate-screen escape codes get swallowed into `$chosen`: the screen stays blank, Ctrl-R "does nothing", and the captured escape soup lands on the command line. (This bit exactly once — the original M6 code used `CrosstermBackend<Stdout>`.)
- **Daemon lifecycle** (done, between M5 and M6) — retired the systemd unit; the `munin` CLI now owns `munind`'s lifecycle. Lazy race-free auto-start (flock + double-checked connect + detached `setsid` spawn + readiness poll) in `munin/src/daemon.rs`, fired by `import`; explicit `munin daemon {start,stop,status,restart,ping}` (ping is a pure probe, no auto-start); `Status`/`Shutdown` IPC ops; self-spawned logs at `$XDG_STATE_HOME/munin/munind.log` (new `levelup_core::xdg::state_file`). The old top-level `munin ping` moved under `daemon`. Added `munin completions [SHELL]` (alias `comp`) via `clap_complete`, defaulting to `$SHELL`. Captures/reads/TUI stay strictly daemon-free. This is the scaffolding sync (M6) needs a daemon to live in.
- **M6** — Sync. Self-hosted server, end-to-end encryption (Argon2id-derived symmetric key), push-unsynced + pull-since loop in `munind` — now with lifecycle in place (auto-start brings the daemon up; `daemon status` is where sync state will surface). Schema is already ready for it (see "Sync columns" in the architecture section above).
- **Later** — MCP server exposing history to AI for "what did I run last week that did X?" queries.
