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
use crate::proto::{EntryMeta, Filters, SearchSort};
use crate::storage;

/// Run the TUI against the SQLite file at `db_path`. Returns the command
/// the user picked (Enter), or `None` if they cancelled (Esc / Ctrl-C).
pub fn run(
    db_path: &Path,
    initial_query: String,
    filters: Filters,
    cfg: &Config,
) -> Result<Option<String>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("open db {}", db_path.display()))?;

    let mut term = setup_terminal()?;
    let result = run_loop(&mut term, &conn, initial_query, filters, cfg);
    // Always tear down the terminal, even on error, so the user gets their
    // shell back instead of a wedged session.
    restore_terminal(&mut term).ok();
    result
}

struct State {
    query: String,
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
) -> Result<Option<String>> {
    let mut state = State {
        query: initial_query,
        sort: cfg.sort_proto(),
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
            KeyOutcome::Accept => return Ok(state.selected_cmd()),
            KeyOutcome::Cancel => return Ok(None),
        }
    }
}

enum KeyOutcome {
    /// Re-render but keep the current results.
    Continue,
    /// Re-run the query (query/sort changed).
    Refresh,
    /// Return the selected command.
    Accept,
    /// Exit without a selection.
    Cancel,
}

fn handle_key(key: KeyEvent, state: &mut State) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => KeyOutcome::Cancel,
        KeyCode::Char('c') if ctrl => KeyOutcome::Cancel,
        KeyCode::Char('d') if ctrl && state.query.is_empty() => KeyOutcome::Cancel,
        KeyCode::Enter => KeyOutcome::Accept,
        KeyCode::Up => {
            state.move_selection(-1);
            KeyOutcome::Continue
        }
        KeyCode::Down => {
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
        // Tab or Ctrl-R cycles sort mode (matches atuin's Ctrl-R convention).
        KeyCode::Tab | KeyCode::Char('r') if matches!(key.code, KeyCode::Tab) || ctrl => {
            state.sort = match state.sort {
                SearchSort::Relevance => SearchSort::Recent,
                SearchSort::Recent => SearchSort::Relevance,
            };
            KeyOutcome::Refresh
        }
        KeyCode::Backspace => {
            if state.query.pop().is_some() {
                KeyOutcome::Refresh
            } else {
                KeyOutcome::Continue
            }
        }
        // Ctrl-U clears the line (readline convention).
        KeyCode::Char('u') if ctrl => {
            if state.query.is_empty() {
                KeyOutcome::Continue
            } else {
                state.query.clear();
                KeyOutcome::Refresh
            }
        }
        // Ctrl-W deletes the previous word.
        KeyCode::Char('w') if ctrl => {
            let trimmed = state.query.trim_end();
            let end = trimmed
                .rfind(char::is_whitespace)
                .map(|i| i + 1)
                .unwrap_or(0);
            if end != state.query.len() {
                state.query.truncate(end);
                KeyOutcome::Refresh
            } else {
                KeyOutcome::Continue
            }
        }
        KeyCode::Char(c) if !ctrl => {
            state.query.push(c);
            KeyOutcome::Refresh
        }
        _ => KeyOutcome::Continue,
    }
}

fn refresh_results(
    state: &mut State,
    conn: &Connection,
    filters: &Filters,
    cfg: &Config,
) -> Result<()> {
    // Empty query → most recent entries (so the TUI opens to something
    // useful before the user types anything). fzf does the same.
    let mut results = if state.query.trim().is_empty() {
        storage::list(conn, cfg.limit, filters)?
    } else {
        storage::search(conn, &state.query, false, state.sort, cfg.limit, filters)?
    };
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

fn render_list(
    f: &mut ratatui::Frame<'_>,
    state: &mut State,
    cfg: &Config,
    area: Rect,
) {
    let match_fg = cfg.colors.match_fg.to_ratatui();
    let items: Vec<ListItem> = state
        .results
        .iter()
        .map(|e| ListItem::new(render_row(e, match_fg)))
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(cfg.colors.selection_fg.to_ratatui())
                .bg(cfg.colors.selection_bg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut state.list_state);
}

/// One row: `<exit>  <duration>  <cmd-with-‹match›-highlights>`.
fn render_row(entry: &EntryMeta, match_fg: ratatui::style::Color) -> Line<'static> {
    let exit = entry
        .exit_code
        .map(|c| format!("{c:>3}"))
        .unwrap_or_else(|| "  -".into());
    let dur = fmt_dur(entry.duration_ms);
    let mut spans = vec![
        Span::raw(format!("{exit}  {dur:>6}  ")),
    ];
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

fn render_status(
    f: &mut ratatui::Frame<'_>,
    state: &State,
    cfg: &Config,
    area: Rect,
) {
    let sort = match state.sort {
        SearchSort::Relevance => "relevance",
        SearchSort::Recent => "recent",
    };
    let text = format!(
        " {n} match{plural}  [{sort}]   Enter accept · Esc cancel · Tab toggle sort",
        n = state.results.len(),
        plural = if state.results.len() == 1 { "" } else { "es" },
    );
    let p = Paragraph::new(text)
        .style(Style::default().fg(cfg.colors.status_fg.to_ratatui()));
    f.render_widget(p, area);
}

fn render_prompt(
    f: &mut ratatui::Frame<'_>,
    state: &State,
    cfg: &Config,
    area: Rect,
) {
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
    // Park the terminal cursor right after the query so the user can see
    // where the next char will land. Use saturating arithmetic — when the
    // pty hasn't negotiated yet (width=0) the naive `area.x + area.width - 1`
    // underflows.
    if area.width > 0 {
        let cursor_x = (area.x + 2).saturating_add(state.query.chars().count() as u16);
        let max_x = area.x.saturating_add(area.width).saturating_sub(1);
        f.set_cursor_position((cursor_x.min(max_x), area.y));
    }
}

fn fmt_dur(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return "-".into() };
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let s = ms / 1000;
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("enter alternate screen")?;
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
