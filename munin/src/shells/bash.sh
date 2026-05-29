# munin shell integration for bash (4.4+).
# Source via:  eval "$(munin init bash)"
#
# Strategy (bash has no native preexec):
#   - PROMPT_COMMAND sets `_munin_pending=1` at the *end* of every prompt
#     cycle. This is run in the parent shell, so the assignment persists.
#   - The DEBUG trap, on the *first* fire of the next cycle, sees the flag,
#     consumes it, and records the start of the command. Subsequent DEBUG
#     fires within the same command (pipeline segments, subshells) skip.
#   - Subshells (BASH_SUBSHELL > 0) are skipped — we only care about
#     top-level interactive commands.
#   - The full command line is read from `builtin history 1`, which captures
#     the original input including pipelines (BASH_COMMAND would only give
#     one segment per DEBUG fire).

[[ -n "${_MUNIN_HOOKED-}" ]] && return 0
_MUNIN_HOOKED=1

: "${MUNIN_SESSION:=$$}"
export MUNIN_SESSION
export MUNIN_SHELL=bash

# Known limitation: bash's `history` strips leading whitespace from entries,
# so commands like ` secret-thing` arrive at the daemon as `secret-thing`.
# The daemon-side leading-whitespace filter (HIST_IGNORE_SPACE convention)
# therefore won't suppress them on bash. Setting `HISTCONTROL=ignorespace`
# in your bashrc keeps them out of bash's history entirely, but then
# `history 1` returns the *previous* line and munin would double-record it.
# A bash-only workaround is on the roadmap; for now, prefix-with-space is a
# zsh-only privacy escape hatch.

_munin_pending=0
_munin_active=0

__munin_preexec() {
    # Only act on the first DEBUG of a top-level interactive command.
    [ "${_munin_pending:-0}" -eq 0 ] && return
    [ "${BASH_SUBSHELL:-0}" -ne 0 ] && return
    _munin_pending=0
    _munin_active=1
    local raw cmd
    raw=$(builtin history 1)
    if [[ "$raw" =~ ^[[:space:]]*[0-9]+[[:space:]]+(.*)$ ]]; then
        cmd="${BASH_REMATCH[1]}"
    else
        cmd="$BASH_COMMAND"
    fi
    # ( cmd & ) detaches the child from this shell's job control so we don't
    # see "Done" notifications; the outer redirect silences any incidentals.
    ( command munin add-start -- "$cmd" "$MUNIN_SESSION" 2>/dev/null & ) >/dev/null 2>&1
}

__munin_precmd() {
    local rc=$?
    if [ "$_munin_active" -eq 1 ]; then
        _munin_active=0
        ( command munin add-end -- "$MUNIN_SESSION" "$rc" 2>/dev/null & ) >/dev/null 2>&1
    fi
    # Arm the DEBUG trap for the next command typed at the prompt.
    _munin_pending=1
    return $rc
}

trap '__munin_preexec' DEBUG
if [[ "${PROMPT_COMMAND-}" != *__munin_precmd* ]]; then
    PROMPT_COMMAND="__munin_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
fi

# Ctrl-R: replace bash's native history-search with the munin picker.
# Exit-code contract from `munin search -i`:
#   0  user hit Enter   → splice the chosen command (see caveat below)
#   2  user hit Tab     → splice the chosen command for editing
#   1  user hit Esc/^C  → leave the line untouched
#
# Caveat: bash's `bind -x` cannot trigger Enter from inside the bound
# function — there is no `accept-line` equivalent reachable from a custom
# readline binding. Both exit 0 and exit 2 therefore land the command on
# the prompt; the user hits Enter to actually run it. zsh's hook honours
# the distinction natively.
_munin_search() {
    local chosen rc
    chosen=$(command munin search -i -- "$READLINE_LINE" </dev/tty 2>/dev/null)
    rc=$?
    case $rc in
      0|2) READLINE_LINE=$chosen; READLINE_POINT=${#READLINE_LINE} ;;
      *)   : ;;
    esac
}
bind -x '"\C-R": _munin_search'
