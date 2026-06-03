# munin shell integration for zsh.
# Source via:  eval "$(munin init zsh)"

[[ -n "${_MUNIN_HOOKED-}" ]] && return 0
_MUNIN_HOOKED=1

: "${MUNIN_SESSION:=$$}"
export MUNIN_SESSION
export MUNIN_SHELL=zsh

_munin_active=0

# Capture runs SYNCHRONOUSLY (no `&!`). `munin` writes straight to SQLite and
# add-end pairs to the session's open row by SQL; if the two calls were
# backgrounded they could reorder, so add-end would close the wrong command
# (scrambling exit codes) and processes could be torn down on shell exit
# (dropping rows). Foreground keeps them in order — within one shell, commands
# are serial, so there's exactly one open row per session at precmd time. The
# cost is ~5 ms per call (process spawn + WAL write); imperceptible.
_munin_preexec() {
    _munin_active=1
    command munin add-start -- "$1" "$MUNIN_SESSION" 2>/dev/null
}

_munin_precmd() {
    local rc=$?
    if (( _munin_active )); then
        _munin_active=0
        command munin add-end -- "$MUNIN_SESSION" "$rc" 2>/dev/null
    fi
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _munin_preexec
add-zsh-hook precmd _munin_precmd

# Ctrl-R: replace zsh's native history-search with the munin picker.
# Exit-code contract from `munin search -i`:
#   0  user hit Enter   → splice the chosen command AND run it
#   2  user hit Tab     → splice the chosen command, leave it for editing
#   1  user hit Esc/^C  → leave the buffer untouched
# stdout is redirected to a temp file (and back through $(...)) so the TUI
# stays attached to /dev/tty for input AND output of its alternate screen.
_munin_search() {
    local chosen rc
    chosen=$(command munin search -i -- "$BUFFER" </dev/tty 2>/dev/null)
    rc=$?
    case $rc in
      0) BUFFER=$chosen; CURSOR=${#BUFFER}; zle accept-line ;;
      2) BUFFER=$chosen; CURSOR=${#BUFFER}; zle reset-prompt ;;
      *) zle reset-prompt ;;
    esac
}
zle -N _munin_search
bindkey '^R' _munin_search
