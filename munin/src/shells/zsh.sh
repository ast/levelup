# munin shell integration for zsh.
# Source via:  eval "$(munin init zsh)"

[[ -n "${_MUNIN_HOOKED-}" ]] && return 0
_MUNIN_HOOKED=1

: "${MUNIN_SESSION:=$$}"
export MUNIN_SESSION
export MUNIN_SHELL=zsh

_munin_active=0

_munin_preexec() {
    _munin_active=1
    command munin add-start -- "$1" "$MUNIN_SESSION" 2>/dev/null &!
}

_munin_precmd() {
    local rc=$?
    if (( _munin_active )); then
        _munin_active=0
        command munin add-end -- "$MUNIN_SESSION" "$rc" 2>/dev/null &!
    fi
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _munin_preexec
add-zsh-hook precmd _munin_precmd
