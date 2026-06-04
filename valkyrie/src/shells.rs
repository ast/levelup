//! Shell hook scripts, embedded at build time. `valkyrie init <shell>` prints
//! the matching script; install with `eval "$(valkyrie init zsh)"`.

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
