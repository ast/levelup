//! Shell hook scripts, embedded at build time.
//!
//! `sleipnir init <shell>` prints the matching script to stdout. The user
//! installs it with `eval "$(sleipnir init zsh)"` in their rc file. Mirrors
//! munin's `shells.rs` scaffolding (`Shell` enum + `include_str!`); adding a
//! shell is one more arm.

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum Shell {
    Zsh,
}

pub fn init_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh => include_str!("shells/zsh.sh"),
    }
}
