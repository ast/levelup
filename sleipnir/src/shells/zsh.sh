# sleipnir shell integration for zsh.
# Source via:  eval "$(sleipnir init zsh)"

[[ -n "${_SLEIPNIR_HOOKED-}" ]] && return 0
_SLEIPNIR_HOOKED=1

# Record every directory you cd into. Runs synchronously (~5 ms: process spawn
# + WAL write); `add` also opportunistically refreshes the file pool mined from
# munin, but only behind a TTL so the common cd stays cheap.
autoload -Uz add-zsh-hook
_sleipnir_chpwd() { command sleipnir add -- "$PWD" 2>/dev/null }
add-zsh-hook chpwd _sleipnir_chpwd

# chpwd does NOT fire for the directory the shell starts in, so record it once
# at source time.
command sleipnir add -- "$PWD" 2>/dev/null

# Ctrl-T: open the modeless frecency picker over your dirs + files.
# Action contract from `sleipnir pick` (one tab-separated line on stdout):
#   cd<TAB>PATH      → jump there         (Enter on a dir / Tab on a file)
#   insert<TAB>PATH  → splice onto the line (Enter on a file / Tab on a dir)
#   (exit 1, no output)                   → cancel (Esc/Ctrl-C)
# stdin is /dev/tty so the picker reads keys there; the picker draws to
# /dev/tty too (never stdout), so $(...) captures only the result line.
_sleipnir_pick() {
    local out action target
    out=$(command sleipnir pick -- "$LBUFFER" </dev/tty 2>/dev/null) || { zle reset-prompt; return }
    IFS=$'\t' read -r action target <<<"$out"
    case $action in
      cd)     builtin cd -- "$target"; zle reset-prompt ;;
      insert) LBUFFER+="${(q)target} "; zle reset-prompt ;;
      *)      zle reset-prompt ;;
    esac
}
zle -N _sleipnir_pick
bindkey '^T' _sleipnir_pick
