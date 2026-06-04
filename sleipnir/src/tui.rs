//! fzf-style modeless picker over the fused dir+file frecency pool.
//!
//! Ported from munin's `tui.rs` (copy-now, extract-later). The core loop,
//! readline keymap, fzf-style bottom layout, `/dev/tty` rendering and `‹›`
//! highlight machinery are unchanged; what's new is the *fused* item model
//! (`storage::Row` with a `Kind`), the dir/file signifier, a preview pane, and
//! the richer action contract (cd vs insert, by row type).

use std::cell::OnceCell;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as LayoutWidget, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use levelup_core::sanitize_display;
use levelup_tui::editing;
use levelup_tui::highlight::highlight_snippet;
use levelup_tui::terminal;

use crate::config::{Config, Layout};
use crate::highlight::Highlighter;
use crate::storage::{self, Kind, Row};

/// What the user chose. The bin layer prints `<action>\t<path>` (the shell
/// widget reads that) for the first two, and exits 1 for `Cancel`.
pub enum Outcome {
    /// Jump to this directory (`builtin cd`).
    Cd(String),
    /// Splice this path onto the command line.
    Insert(String),
    /// Esc / Ctrl-C / Ctrl-G, or accept with no selection.
    Cancel,
}

/// Run the picker over a pre-built `pool` (the caller fuses frecency + the live
/// walk), seeded with `initial_query`. The pool is built once up front, so
/// per-keystroke scoring stays in memory.
pub fn run(pool: Vec<Row>, initial_query: String, cfg: &Config) -> Result<Outcome> {
    let mut term = terminal::setup()?;
    let result = run_loop(&mut term, pool, initial_query, cfg);
    // Always tear the terminal down so the user gets their shell back.
    terminal::restore(&mut term).ok();
    result
}

struct State {
    query: String,
    /// Byte offset into `query`, always on a UTF-8 char boundary.
    cursor: usize,
    /// Immutable base pool (loaded once); `results` is the ranked view.
    pool: Vec<Row>,
    results: Vec<Row>,
    list_state: ListState,
    /// Syntax highlighter for file previews, built lazily on first file preview
    /// (so the ~23ms syntect load never blocks startup or affects typing).
    highlighter: OnceCell<Highlighter>,
}

impl State {
    fn select(&mut self, idx: Option<usize>) {
        self.list_state.select(idx);
    }

    fn selected_row(&self) -> Option<&Row> {
        self.list_state.selected().and_then(|i| self.results.get(i))
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
    term: &mut Terminal<CrosstermBackend<File>>,
    pool: Vec<Row>,
    initial_query: String,
    cfg: &Config,
) -> Result<Outcome> {
    let cursor = initial_query.len();
    let mut state = State {
        query: initial_query,
        cursor,
        pool,
        results: Vec::new(),
        list_state: ListState::default(),
        highlighter: OnceCell::new(),
    };
    refresh_results(&mut state, cfg);

    loop {
        term.draw(|f| render(f, &mut state, cfg))?;

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
            KeyOutcome::Refresh => refresh_results(&mut state, cfg),
            KeyOutcome::Accept => return Ok(accept(state.selected_row(), false)),
            KeyOutcome::AcceptInverse => return Ok(accept(state.selected_row(), true)),
            KeyOutcome::Cancel => return Ok(Outcome::Cancel),
        }
    }
}

/// Map (selected row, key) → action. Enter acts by type (dir → cd, file →
/// insert); Tab is the inverse (dir → insert as an arg, file → cd to parent).
fn accept(row: Option<&Row>, inverse: bool) -> Outcome {
    let Some(row) = row else {
        return Outcome::Cancel;
    };
    match (row.kind, inverse) {
        (Kind::Dir, false) => Outcome::Cd(row.path.clone()),
        (Kind::File, false) => Outcome::Insert(row.path.clone()),
        (Kind::Dir, true) => Outcome::Insert(row.path.clone()),
        (Kind::File, true) => Outcome::Cd(parent_of(&row.path)),
    }
}

fn parent_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

enum KeyOutcome {
    Continue,
    Refresh,
    Accept,
    AcceptInverse,
    Cancel,
}

fn handle_key(key: KeyEvent, state: &mut State) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // ---- exit / accept ----------------------------------------------
        KeyCode::Esc => KeyOutcome::Cancel,
        KeyCode::Char('c') if ctrl => KeyOutcome::Cancel,
        KeyCode::Char('g') if ctrl => KeyOutcome::Cancel,
        KeyCode::Char('d') if ctrl => {
            if state.query.is_empty() {
                KeyOutcome::Cancel
            } else {
                delete_forward_char(state)
            }
        }
        KeyCode::Enter => KeyOutcome::Accept,
        KeyCode::Tab => KeyOutcome::AcceptInverse,

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
        KeyCode::Backspace => delete_back_char(state),
        KeyCode::Char('h') if ctrl => delete_back_char(state),
        KeyCode::Char('k') if ctrl => kill_to_end(state),
        KeyCode::Char('u') if ctrl => {
            if state.query.is_empty() {
                KeyOutcome::Continue
            } else {
                state.query.clear();
                state.cursor = 0;
                KeyOutcome::Refresh
            }
        }
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

// ---- editing helpers (verbatim from munin) --------------------------------

fn refresh_if(changed: bool) -> KeyOutcome {
    if changed {
        KeyOutcome::Refresh
    } else {
        KeyOutcome::Continue
    }
}

fn move_cursor_back(state: &mut State) -> KeyOutcome {
    state.cursor = editing::prev_offset(&state.query, state.cursor);
    KeyOutcome::Continue
}

fn move_cursor_forward(state: &mut State) -> KeyOutcome {
    state.cursor = editing::next_offset(&state.query, state.cursor);
    KeyOutcome::Continue
}

fn delete_back_char(state: &mut State) -> KeyOutcome {
    refresh_if(editing::delete_back(&mut state.query, &mut state.cursor))
}

fn delete_forward_char(state: &mut State) -> KeyOutcome {
    refresh_if(editing::delete_forward(&mut state.query, &mut state.cursor))
}

fn kill_to_end(state: &mut State) -> KeyOutcome {
    refresh_if(editing::kill_to_end(&mut state.query, &mut state.cursor))
}

fn delete_back_word(state: &mut State) -> KeyOutcome {
    refresh_if(editing::delete_word(&mut state.query, &mut state.cursor))
}

fn refresh_results(state: &mut State, cfg: &Config) {
    let mut results = storage::rank_pool(state.pool.clone(), &state.query);
    results.truncate(cfg.limit);
    // fzf-style bottom layout wants the BEST match nearest the prompt. ratatui
    // renders top→down, so reverse: index 0 = worst (top), len-1 = best (just
    // above the prompt).
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
    *state.list_state.offset_mut() = 0;
    state.select(initial);
}

fn render(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config) {
    let area = f.area();
    // Split off a preview pane on the right when enabled and there's room.
    let (main_area, preview_area) = if cfg.preview && area.width >= 80 {
        let cols = LayoutWidget::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

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
        .split(main_area);

    let (list_area, status_area, prompt_area) = match cfg.layout {
        Layout::Bottom => (chunks[0], chunks[1], chunks[2]),
        Layout::Top => (chunks[2], chunks[1], chunks[0]),
    };

    render_list(f, state, cfg, list_area);
    render_status(f, state, cfg, status_area);
    render_prompt(f, state, cfg, prompt_area);
    if let Some(pa) = preview_area {
        render_preview(f, state, cfg, pa);
    }
}

fn render_list(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config, area: Rect) {
    let match_fg = cfg.colors.match_fg.to_ratatui();
    let dir_fg = cfg.colors.dir_fg.to_ratatui();
    let file_fg = cfg.colors.file_fg.to_ratatui();
    let items: Vec<ListItem> = state
        .results
        .iter()
        .map(|r| ListItem::new(render_row(r, match_fg, dir_fg, file_fg)))
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

    // For Bottom layout, keep the items flush against the prompt: sub-split so
    // a short list sits at the bottom of its slot (see munin's tui for the why).
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

/// One row: the home-abbreviated path, coloured by kind, with a trailing `/`
/// on directories. The trailing slash is a shape cue that doesn't rely on
/// colour alone (Norman: don't signify with colour only).
fn render_row(row: &Row, match_fg: Color, dir_fg: Color, file_fg: Color) -> Line<'static> {
    let base = match row.kind {
        Kind::Dir => dir_fg,
        Kind::File => file_fg,
    };
    let mut spans = match row.snippet.as_deref() {
        Some(snippet) => highlight_snippet(snippet, match_fg, base),
        None => vec![Span::styled(
            sanitize_display(&row.display),
            Style::default().fg(base),
        )],
    };
    if row.kind == Kind::Dir {
        spans.push(Span::styled("/", Style::default().fg(base)));
    }
    Line::from(spans)
}

fn render_status(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    // Hint reflects what Enter/Tab will do for the *current* selection — the
    // action is modeless (no toggle) but its effect depends on the row type,
    // so we spell it out (Norman: feedback / visible mapping).
    let hint = match state.selected_row().map(|r| r.kind) {
        Some(Kind::Dir) => "Enter cd · Tab insert",
        Some(Kind::File) => "Enter insert · Tab cd-parent",
        None => "no match",
    };
    let text = format!(
        " {n} result{plural}   {hint} · Esc cancel",
        n = state.results.len(),
        plural = if state.results.len() == 1 { "" } else { "s" },
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
    if area.width > 0 {
        let chars_before = state.query[..state.cursor].chars().count() as u16;
        let cursor_x = (area.x + 2).saturating_add(chars_before);
        let max_x = area.x.saturating_add(area.width).saturating_sub(1);
        f.set_cursor_position((cursor_x.min(max_x), area.y));
    }
}

/// Right-side feedback: for a dir, its immediate entries; for a file, its first
/// lines. Best-effort and capped — a preview must never block or blow up the
/// picker, so every error just yields an empty/short pane.
fn render_preview(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(cfg.colors.status_fg.to_ratatui()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(row) = state.selected_row() else {
        return;
    };
    let max_rows = inner.height as usize;
    let lines: Vec<Line> = match row.kind {
        Kind::Dir => preview_dir(&row.path, max_rows)
            .into_iter()
            .map(|s| Line::from(sanitize_display(&s)))
            .collect(),
        Kind::File => {
            let raw = preview_file(&row.path, max_rows);
            // Lazily build the highlighter on the first file preview, then reuse
            // it for the session. Unknown languages fall back to plain text.
            let hl = state.highlighter.get_or_init(Highlighter::new);
            hl.highlight(&row.path, &raw).unwrap_or_else(|| {
                raw.into_iter()
                    .map(|s| Line::from(sanitize_display(&s)))
                    .collect()
            })
        }
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn preview_dir(path: &str, max_rows: usize) -> Vec<String> {
    let mut entries: Vec<String> = match std::fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect(),
        Err(e) => return vec![format!("(cannot read dir: {e})")],
    };
    entries.sort();
    entries.truncate(max_rows);
    entries
}

fn preview_file(path: &str, max_rows: usize) -> Vec<String> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) => return vec![format!("(cannot read file: {e})")],
    };
    BufReader::new(f)
        .lines()
        .take(max_rows)
        // Lossy/!UTF-8 or read errors → stop early rather than erroring out.
        .map_while(|l| l.ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_row(path: &str) -> Row {
        Row {
            path: path.into(),
            display: path.into(),
            kind: Kind::Dir,
            frecency: 1.0,
            snippet: None,
        }
    }
    fn file_row(path: &str) -> Row {
        Row {
            path: path.into(),
            display: path.into(),
            kind: Kind::File,
            frecency: 1.0,
            snippet: None,
        }
    }

    #[test]
    fn enter_acts_by_kind() {
        let d = dir_row("/a/b");
        let fr = file_row("/a/b/c.txt");
        assert!(matches!(accept(Some(&d), false), Outcome::Cd(p) if p == "/a/b"));
        assert!(matches!(accept(Some(&fr), false), Outcome::Insert(p) if p == "/a/b/c.txt"));
    }

    #[test]
    fn tab_is_the_inverse() {
        let d = dir_row("/a/b");
        let fr = file_row("/a/b/c.txt");
        assert!(matches!(accept(Some(&d), true), Outcome::Insert(p) if p == "/a/b"));
        // File → cd to its parent directory.
        assert!(matches!(accept(Some(&fr), true), Outcome::Cd(p) if p == "/a/b"));
    }

    #[test]
    fn accept_without_selection_cancels() {
        assert!(matches!(accept(None, false), Outcome::Cancel));
    }
}
