//! Alternate-screen terminal setup/teardown on `/dev/tty`.

use std::fs::File;
use std::io::Write;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
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

/// Negotiate the Kitty keyboard protocol so held keys report real
/// press/repeat/**release** events — what a steady, jitter-free hold-to-confirm
/// meter needs (terminals' classic protocol reports no key-up, forcing a
/// flicker-prone repeat-timeout heuristic). Returns whether it was enabled, so
/// callers know to trust release events; `false` on terminals without support,
/// where the caller falls back to the timeout. We push `DISAMBIGUATE_ESCAPE_CODES`
/// (so modified keys like Alt-K arrive as CSI-u carrying an event type) plus
/// `REPORT_EVENT_TYPES`. Pair with [`disable_keyboard_enhancement`] on teardown.
pub fn enable_keyboard_enhancement<W: Write>(term: &mut Terminal<CrosstermBackend<W>>) -> bool {
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        if execute!(term.backend_mut(), PushKeyboardEnhancementFlags(flags)).is_ok() {
            return true;
        }
    }
    false
}

/// Pop the flags pushed by [`enable_keyboard_enhancement`] (only when `enabled`),
/// so the terminal isn't left in enhanced mode.
pub fn disable_keyboard_enhancement<W: Write>(
    term: &mut Terminal<CrosstermBackend<W>>,
    enabled: bool,
) {
    if enabled {
        let _ = execute!(term.backend_mut(), PopKeyboardEnhancementFlags);
    }
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
