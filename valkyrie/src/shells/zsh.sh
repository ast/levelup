# valkyrie shell integration for zsh.
# Source via:  eval "$(valkyrie init zsh)"

[[ -n "${_VALKYRIE_HOOKED-}" ]] && return 0
_VALKYRIE_HOOKED=1

# `vk` for a quick non-bound launch.
alias vk='valkyrie pick'

# Alt-P: summon the chooser of the slain. Unlike munin/sleipnir this widget
# does NOT splice the command line — Valkyrie acts on the world (sends signals)
# itself — so we just run it on /dev/tty and redraw the prompt afterwards.
_valkyrie_pick() {
    command valkyrie pick </dev/tty
    zle reset-prompt
}
zle -N _valkyrie_pick
bindkey '^[p' _valkyrie_pick
