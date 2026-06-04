//! Alternate-screen terminal setup/teardown on `/dev/tty`.

use std::fs::File;
use std::io::Write;

use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// Set up a full-screen picker rendered to `/dev/tty` (NOT stdout): the shell
/// hooks capture the binary's stdout for the chosen result, so drawing the
/// alternate screen to stdout would be swallowed into that capture. crossterm
/// reads key events from `/dev/tty` too, so input is unaffected.
pub fn setup() -> Result<Terminal<CrosstermBackend<File>>> {
    let mut tty = File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("open /dev/tty")?;
    enable_raw_mode().context("enable raw mode")?;
    execute!(tty, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let _ = tty.flush();
    Terminal::new(CrosstermBackend::new(tty)).context("create terminal")
}

/// Tear the alternate screen down and restore the cursor. Generic over the
/// backend's writer, so it restores a stdout-backed terminal too.
pub fn restore<W: Write>(term: &mut Terminal<CrosstermBackend<W>>) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("leave alternate screen")?;
    term.show_cursor().context("show cursor")?;
    Ok(())
}
