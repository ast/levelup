# Show available recipes
default:
    @just --list

# --- Workspace ---

# Type-check everything
check:
    cargo check --workspace

# Debug build
build:
    cargo build --workspace

# Release build
release:
    cargo build --workspace --release

# Clippy with warnings treated as errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Format every Rust file
fmt:
    cargo fmt --all

# Verify formatting without modifying
fmt-check:
    cargo fmt --all -- --check

# --- munin ---

# Run the munin CLI (forward args after `--`, e.g. `just munin -- search foo`)
munin *ARGS:
    cargo run --quiet --bin munin -p munin -- {{ARGS}}

# Run the munind daemon. MUNIN_LOG controls log level.
munind:
    cargo run --bin munind -p munin

# --- mimir ---

# Run the mimir status bar (forward args after `--`)
mimir *ARGS:
    cargo run --quiet -p mimir -- {{ARGS}}

# --- sleipnir ---

# Run the sleipnir frecency navigator (forward args after `--`, e.g. `just sleipnir -- query`)
sleipnir *ARGS:
    cargo run --quiet -p sleipnir -- {{ARGS}}

# --- valkyrie ---

# Run the valkyrie process finder (forward args after `--`, e.g. `just valkyrie -- list`)
valkyrie *ARGS:
    cargo run --quiet -p valkyrie -- {{ARGS}}

# --- heimdall ---

# Run the heimdall LAN device finder (forward args after `--`, e.g. `just heimdall -- scan`)
heimdall *ARGS:
    cargo run --quiet -p heimdall -- {{ARGS}}

# --- hugin ---

# Run hugind against your real database. HUGIN_LOG controls log level.
run:
    cargo run --bin hugind -p hugin

# Inspect the default hugin database
schema:
    cargo run --example schema -p hugin

# Inspect a specific database file
schema-of DB:
    cargo run --example schema -p hugin -- {{DB}}

# Run the hugin CLI (forward args after `--`, e.g. `just hugin -- list`)
hugin *ARGS:
    cargo run --quiet --bin hugin -p hugin -- {{ARGS}}

# Install every tool's binaries to ~/.cargo/bin (the paths the dist/*.service units expect)
install:
    cargo install --path hugin --locked
    cargo install --path munin --locked
    cargo install --path mimir --locked
    cargo install --path sleipnir --locked
    cargo install --path valkyrie --locked
    cargo install --path heimdall --locked

# Print a tool's shell completions for SHELL (bash/zsh/fish/elvish/powershell).
# Works for any CLI: hugin, munin, mimir, sleipnir, valkyrie, heimdall.
completions TOOL SHELL:
    @cargo run --quiet --bin {{TOOL}} -p {{TOOL}} -- completions {{SHELL}}

# Print hugind daemon completions for SHELL (the daemon uses a flag, not a subcommand)
completions-hugind SHELL:
    @cargo run --quiet --bin hugind -p hugin -- --generate-completions {{SHELL}}

# End-to-end smoke test against a temp DB + socket (overwrites your clipboard; needs wl-clipboard)
smoke: build
    #!/usr/bin/env bash
    set -uo pipefail
    DB=/tmp/hugin-smoke.db
    SOCK=/tmp/hugin-smoke.sock
    LOG=/tmp/hugin-smoke.log
    rm -f "$DB" "$DB-wal" "$DB-shm" "$SOCK" "$LOG"

    # --primary so the primary-selection step actually exercises the watcher.
    echo ">> starting hugind --primary (log: $LOG)"
    HUGIN_LOG=debug ./target/debug/hugind --db "$DB" --socket "$SOCK" --primary 2>"$LOG" &
    PID=$!
    trap 'kill $PID 2>/dev/null; wait $PID 2>/dev/null' EXIT
    sleep 0.5
    if ! kill -0 $PID 2>/dev/null; then
        echo "hugind exited at startup:"
        cat "$LOG"
        exit 1
    fi

    echo ">> regular: 'hello hugin'"
    printf 'hello hugin' | wl-copy
    sleep 0.15

    echo ">> regular: 'second copy' twice (second should dedup)"
    printf 'second copy' | wl-copy; sleep 0.15
    printf 'second copy' | wl-copy; sleep 0.15

    echo ">> primary: 'primary one'"
    printf 'primary one' | wl-copy --primary
    sleep 0.2

    echo ">> password-manager hint: 'secret' (should be skipped)"
    printf 'secret' | wl-copy --type x-kde-passwordManagerHint
    sleep 0.2

    echo ">> ipc: ping"
    ./target/debug/hugin --socket "$SOCK" ping

    echo ">> ipc: list"
    ./target/debug/hugin --socket "$SOCK" list

    # Pick the 'hello hugin' entry: second-newest regular before the dedup'd 'second copy'.
    HELLO_ID=$(./target/debug/hugin --socket "$SOCK" list --selection regular --limit 2 | tail -1 | awk '{print $1}')
    echo ">> ipc: info $HELLO_ID"
    ./target/debug/hugin --socket "$SOCK" info "$HELLO_ID"

    echo ">> ipc: copy $HELLO_ID (daemon becomes source)"
    ./target/debug/hugin --socket "$SOCK" copy "$HELLO_ID"
    sleep 0.3
    BACK=$(timeout 2 wl-paste --no-newline 2>/dev/null || echo "<wl-paste timed out>")
    echo "   wl-paste reads back: '$BACK' (expected 'hello hugin')"

    echo ">> SIGTERM (graceful shutdown)"
    kill -TERM $PID
    wait $PID
    EXIT=$?
    trap - EXIT
    echo "   exit status: $EXIT (expected 0)"

    echo
    stored=$( grep -c 'stored '   "$LOG" || true)
    dedup=$(  grep -c 'dedup:'    "$LOG" || true)
    skipped=$(grep -c 'skipping clipboard content marked' "$LOG" || true)
    echo "summary:"
    echo "  stored=$stored   (expected 3: hello, second, primary)"
    echo "  dedup=$dedup    (expected 1: the repeated 'second copy')"
    echo "  skipped=$skipped (expected 1: the password-manager hint)"
    echo "  copy round-trip: '$BACK' (expected 'hello hugin')"
    echo "  clean-shutdown exit: $EXIT (expected 0)"
    echo "  (extra 'stored' entries = whatever was on your clipboard at startup)"
