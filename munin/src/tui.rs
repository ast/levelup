//! fzf-style interactive search TUI.
//!
//! Renders a list of search results above a prompt at the bottom of the
//! screen. Type → re-runs the search on every keystroke. Up/Down → move
//! the selection. Enter → return the selected command (the bin layer
//! prints it to stdout so the shell hook can splice it into the line
//! buffer). Esc / Ctrl-C → return `None`.
//!
//! Reads go directly against the SQLite file (WAL mode makes this safe
//! alongside the daemon's writes) instead of round-tripping through IPC —
//! the TUI is a short-lived read-only client and per-keystroke IPC would
//! add unnecessary latency.

use std::io::{self, Stdout};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as LayoutWidget, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use rusqlite::Connection;

use crate::config::{Config, Layout};
use crate::fmt_dur;
use crate::proto::{EntryMeta, Filters, SearchSort};
use crate::storage;

/// What the user did. The bin layer maps each variant to a different exit
/// code so the shell hook can distinguish "run this" from "drop this on the
/// command line for me to edit" from "cancel".
pub enum Outcome {
    /// Enter: run the chosen command immediately. Bin exits 0.
    Run(String),
    /// Tab: drop the chosen command on the command line; the user edits and
    /// runs it themselves. Bin exits 2.
    Edit(String),
    /// Esc / Ctrl-C / Ctrl-D on empty query: do nothing. Bin exits 1.
    Cancel,
}

/// Run the TUI against the SQLite file at `db_path`.
pub fn run(
    db_path: &Path,
    initial_query: String,
    filters: Filters,
    cfg: &Config,
) -> Result<Outcome> {
    let conn =
        Connection::open(db_path).with_context(|| format!("open db {}", db_path.display()))?;

    let mut term = setup_terminal()?;
    let result = run_loop(&mut term, &conn, initial_query, filters, cfg);
    // Always tear down the terminal, even on error, so the user gets their
    // shell back instead of a wedged session.
    restore_terminal(&mut term).ok();
    result
}

struct State {
    query: String,
    /// Byte offset into `query`. Always on a UTF-8 char boundary; all
    /// mutating handlers maintain this invariant.
    cursor: usize,
    sort: SearchSort,
    results: Vec<EntryMeta>,
    list_state: ListState,
}

impl State {
    fn select(&mut self, idx: Option<usize>) {
        self.list_state.select(idx);
    }

    fn selected_cmd(&self) -> Option<String> {
        self.list_state
            .selected()
            .and_then(|i| self.results.get(i))
            .map(|e| e.cmd.clone())
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            self.list_state.select(None);
            return;
        }
        let len = self.results.len() as isize;
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len - 1);
        self.list_state.select(Some(next as usize));
    }
}

fn run_loop(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    conn: &Connection,
    initial_query: String,
    filters: Filters,
    cfg: &Config,
) -> Result<Outcome> {
    let cursor = initial_query.len();
    let mut state = State {
        query: initial_query,
        cursor,
        sort: cfg.sort,
        results: Vec::new(),
        list_state: ListState::default(),
    };
    refresh_results(&mut state, conn, &filters, cfg)?;

    loop {
        term.draw(|f| render(f, &mut state, cfg))?;

        // 100ms poll lets us pick up window resizes without busy-looping.
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match handle_key(key, &mut state) {
            KeyOutcome::Continue => {}
            KeyOutcome::Refresh => refresh_results(&mut state, conn, &filters, cfg)?,
            KeyOutcome::Accept => {
                return Ok(state
                    .selected_cmd()
                    .map(Outcome::Run)
                    .unwrap_or(Outcome::Cancel));
            }
            KeyOutcome::AcceptEdit => {
                return Ok(state
                    .selected_cmd()
                    .map(Outcome::Edit)
                    .unwrap_or(Outcome::Cancel));
            }
            KeyOutcome::Cancel => return Ok(Outcome::Cancel),
        }
    }
}

enum KeyOutcome {
    /// Re-render but keep the current results.
    Continue,
    /// Re-run the query (query/sort changed).
    Refresh,
    /// Return the selected command for immediate execution.
    Accept,
    /// Return the selected command for editing (don't execute).
    AcceptEdit,
    /// Exit without a selection.
    Cancel,
}

fn handle_key(key: KeyEvent, state: &mut State) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // ---- exit / accept ----------------------------------------------
        KeyCode::Esc => KeyOutcome::Cancel,
        KeyCode::Char('c') if ctrl => KeyOutcome::Cancel,
        KeyCode::Char('g') if ctrl => KeyOutcome::Cancel,
        // Ctrl-D on empty: cancel (readline convention). Non-empty: delete
        // the char under the cursor (Emacs `delete-char`).
        KeyCode::Char('d') if ctrl => {
            if state.query.is_empty() {
                KeyOutcome::Cancel
            } else {
                delete_forward_char(state)
            }
        }
        KeyCode::Enter => KeyOutcome::Accept,
        KeyCode::Tab => KeyOutcome::AcceptEdit,

        // ---- list navigation --------------------------------------------
        KeyCode::Up => {
            state.move_selection(-1);
            KeyOutcome::Continue
        }
        KeyCode::Down => {
            state.move_selection(1);
            KeyOutcome::Continue
        }
        KeyCode::Char('p') if ctrl => {
            state.move_selection(-1);
            KeyOutcome::Continue
        }
        KeyCode::Char('n') if ctrl => {
            state.move_selection(1);
            KeyOutcome::Continue
        }
        KeyCode::PageUp => {
            state.move_selection(-10);
            KeyOutcome::Continue
        }
        KeyCode::PageDown => {
            state.move_selection(10);
            KeyOutcome::Continue
        }

        // ---- sort toggle ------------------------------------------------
        // Atuin's convention: Ctrl-R cycles relevance ↔ recent. Tab is
        // intentionally NOT bound to this any more (it now triggers Edit).
        KeyCode::Char('r') if ctrl => {
            state.sort = match state.sort {
                SearchSort::Relevance => SearchSort::Recent,
                SearchSort::Recent => SearchSort::Relevance,
            };
            KeyOutcome::Refresh
        }

        // ---- cursor movement (Emacs / readline) -------------------------
        KeyCode::Left => move_cursor_back(state),
        KeyCode::Right => move_cursor_forward(state),
        KeyCode::Char('b') if ctrl => move_cursor_back(state),
        KeyCode::Char('f') if ctrl => move_cursor_forward(state),
        KeyCode::Home | KeyCode::Char('a') if matches!(key.code, KeyCode::Home) || ctrl => {
            state.cursor = 0;
            KeyOutcome::Continue
        }
        KeyCode::End | KeyCode::Char('e') if matches!(key.code, KeyCode::End) || ctrl => {
            state.cursor = state.query.len();
            KeyOutcome::Continue
        }

        // ---- editing ----------------------------------------------------
        // Backspace and Ctrl-H are aliases — some terminals send 0x08
        // (Ctrl-H) for the Backspace key, others send 0x7f (DEL).
        KeyCode::Backspace => delete_back_char(state),
        KeyCode::Char('h') if ctrl => delete_back_char(state),
        KeyCode::Char('k') if ctrl => kill_to_end(state),
        // Ctrl-U: kill the whole line (readline convention; Emacs binds
        // this to negative argument, but readline / atuin / fzf all use it
        // for line-kill which is what users expect here).
        KeyCode::Char('u') if ctrl => {
            if state.query.is_empty() {
                KeyOutcome::Continue
            } else {
                state.query.clear();
                state.cursor = 0;
                KeyOutcome::Refresh
            }
        }
        // Ctrl-W: delete previous word from the cursor.
        KeyCode::Char('w') if ctrl => delete_back_word(state),

        // ---- char input -------------------------------------------------
        KeyCode::Char(c) if !ctrl => {
            state.query.insert(state.cursor, c);
            state.cursor += c.len_utf8();
            KeyOutcome::Refresh
        }
        _ => KeyOutcome::Continue,
    }
}

// ---- editing helpers ------------------------------------------------------

fn prev_char_offset(s: &str, pos: usize) -> usize {
    s[..pos]
        .chars()
        .next_back()
        .map_or(0, |c| pos - c.len_utf8())
}

fn next_char_offset(s: &str, pos: usize) -> usize {
    s[pos..]
        .chars()
        .next()
        .map_or(s.len(), |c| pos + c.len_utf8())
}

fn move_cursor_back(state: &mut State) -> KeyOutcome {
    state.cursor = prev_char_offset(&state.query, state.cursor);
    KeyOutcome::Continue
}

fn move_cursor_forward(state: &mut State) -> KeyOutcome {
    state.cursor = next_char_offset(&state.query, state.cursor);
    KeyOutcome::Continue
}

fn delete_back_char(state: &mut State) -> KeyOutcome {
    if state.cursor == 0 {
        return KeyOutcome::Continue;
    }
    let prev = prev_char_offset(&state.query, state.cursor);
    state.query.replace_range(prev..state.cursor, "");
    state.cursor = prev;
    KeyOutcome::Refresh
}

fn delete_forward_char(state: &mut State) -> KeyOutcome {
    if state.cursor >= state.query.len() {
        return KeyOutcome::Continue;
    }
    let next = next_char_offset(&state.query, state.cursor);
    state.query.replace_range(state.cursor..next, "");
    KeyOutcome::Refresh
}

fn kill_to_end(state: &mut State) -> KeyOutcome {
    if state.cursor >= state.query.len() {
        return KeyOutcome::Continue;
    }
    state.query.truncate(state.cursor);
    KeyOutcome::Refresh
}

fn delete_back_word(state: &mut State) -> KeyOutcome {
    if state.cursor == 0 {
        return KeyOutcome::Continue;
    }
    // Find the start of the previous "word", skipping trailing whitespace
    // first so " foo bar " + cursor-at-end yields "foo ".
    let prefix = &state.query[..state.cursor];
    let trimmed = prefix.trim_end();
    let new_start = trimmed
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    if new_start == state.cursor {
        return KeyOutcome::Continue;
    }
    state.query.replace_range(new_start..state.cursor, "");
    state.cursor = new_start;
    KeyOutcome::Refresh
}

fn refresh_results(
    state: &mut State,
    conn: &Connection,
    filters: &Filters,
    cfg: &Config,
) -> Result<()> {
    // `storage::search` handles the empty-query short-circuit internally
    // (falls back to most-recent N), so we don't branch here. Non-empty
    // queries go through nucleo fuzzy matching.
    let mut results = storage::search(conn, &state.query, state.sort, cfg.limit, filters)?;
    // For fzf-style bottom layout we want the BEST/NEWEST match visually
    // nearest the prompt. ratatui's List renders top→down, so reverse the
    // vector: index 0 ends up at the top of the list area (worst/oldest),
    // index len-1 sits right above the prompt (best/newest).
    if matches!(cfg.layout, Layout::Bottom) {
        results.reverse();
    }
    state.results = results;
    let initial = if state.results.is_empty() {
        None
    } else {
        Some(match cfg.layout {
            Layout::Bottom => state.results.len() - 1,
            Layout::Top => 0,
        })
    };
    // Reset the scroll offset before re-selecting. Otherwise a stale offset
    // from a previous (larger) result set leaves the now-tiny list scrolled
    // partway down its own slot, with empty rows visible between the items
    // and the prompt.
    *state.list_state.offset_mut() = 0;
    state.select(initial);
    Ok(())
}

fn render(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config) {
    let area = f.area();
    let chunks = LayoutWidget::default()
        .direction(Direction::Vertical)
        .constraints(match cfg.layout {
            Layout::Bottom => [
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ],
            Layout::Top => [
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ],
        })
        .split(area);

    let (list_area, status_area, prompt_area) = match cfg.layout {
        Layout::Bottom => (chunks[0], chunks[1], chunks[2]),
        Layout::Top => (chunks[2], chunks[1], chunks[0]),
    };

    render_list(f, state, cfg, list_area);
    render_status(f, state, cfg, status_area);
    render_prompt(f, state, cfg, prompt_area);
}

fn render_list(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config, area: Rect) {
    let match_fg = cfg.colors.match_fg.to_ratatui();
    let items: Vec<ListItem> = state
        .results
        .iter()
        .map(|e| ListItem::new(render_row(e, match_fg)))
        .collect();
    let item_count = items.len() as u16;
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(cfg.colors.selection_fg.to_ratatui())
                .bg(cfg.colors.selection_bg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    // For Layout::Bottom (fzf-style) we want the items to sit flush against
    // the bottom of the list area so the best match stays one row above the
    // prompt no matter how many results we have. ratatui's List renders
    // top→down, so when item_count < area.height we sub-split the area into
    // [flexible top spacer | exactly-item_count rows] and render into the
    // bottom slot. When item_count >= area.height the list fills the whole
    // area and ratatui's normal scrolling kicks in.
    let list_area = match cfg.layout {
        Layout::Bottom if item_count < area.height => {
            let chunks = LayoutWidget::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(item_count)])
                .split(area);
            chunks[1]
        }
        _ => area,
    };
    f.render_stateful_widget(list, list_area, &mut state.list_state);
}

/// One row: `<exit>  <duration>  <cmd-with-‹match›-highlights>`.
fn render_row(entry: &EntryMeta, match_fg: ratatui::style::Color) -> Line<'static> {
    let exit = entry
        .exit_code
        .map(|c| format!("{c:>3}"))
        .unwrap_or_else(|| "  -".into());
    let dur = fmt_dur(entry.duration_ms);
    let mut spans = vec![Span::raw(format!("{exit}  {dur:>6}  "))];
    // Search results carry a snippet with `‹›` markers. Walk the snippet and
    // colour the matched runs. List/get results (no snippet) just show cmd.
    if let Some(snippet) = entry.snippet.as_deref() {
        spans.extend(highlight_snippet(snippet, match_fg));
    } else {
        spans.push(Span::raw(entry.cmd.replace('\n', "\u{21B5}")));
    }
    Line::from(spans)
}

fn highlight_snippet(s: &str, match_fg: ratatui::style::Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut in_match = false;
    for ch in s.chars() {
        match ch {
            '‹' => {
                if !buf.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut buf)));
                }
                in_match = true;
            }
            '›' => {
                if !buf.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        Style::default().fg(match_fg).add_modifier(Modifier::BOLD),
                    ));
                }
                in_match = false;
            }
            '\n' => buf.push('\u{21B5}'),
            c => buf.push(c),
        }
    }
    if !buf.is_empty() {
        if in_match {
            spans.push(Span::styled(
                buf,
                Style::default().fg(match_fg).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw(buf));
        }
    }
    spans
}

fn render_status(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let sort = match state.sort {
        SearchSort::Relevance => "relevance",
        SearchSort::Recent => "recent",
    };
    let text = format!(
        " {n} match{plural}  [{sort}]   Enter run · Tab edit · Esc cancel · ^R toggle sort",
        n = state.results.len(),
        plural = if state.results.len() == 1 { "" } else { "es" },
    );
    let p = Paragraph::new(text).style(Style::default().fg(cfg.colors.status_fg.to_ratatui()));
    f.render_widget(p, area);
}

fn render_prompt(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "› ",
            Style::default()
                .fg(cfg.colors.prompt_fg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(state.query.clone()),
    ]);
    f.render_widget(Paragraph::new(line), area);
    // Park the terminal cursor at the *byte* offset stored in `state.cursor`,
    // converted to a column. Use saturating arithmetic — when the pty hasn't
    // negotiated yet (width=0) the naive `area.x + area.width - 1` underflows.
    if area.width > 0 {
        let chars_before = state.query[..state.cursor].chars().count() as u16;
        let cursor_x = (area.x + 2).saturating_add(chars_before);
        let max_x = area.x.saturating_add(area.width).saturating_sub(1);
        f.set_cursor_position((cursor_x.min(max_x), area.y));
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("create terminal")
}

fn restore_terminal(term: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
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
