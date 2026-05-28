//! Shell hook scripts, embedded at build time.
//!
//! `munin init <shell>` prints the matching script to stdout. The user
//! installs it by adding `eval "$(munin init zsh)"` (or `bash`) to their
//! rc file. Adding a new shell is one more `include_str!` + one match arm.

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum Shell {
    Zsh,
    Bash,
}

pub fn init_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh => include_str!("shells/zsh.sh"),
        Shell::Bash => include_str!("shells/bash.sh"),
    }
}
