//! fzf-style interactive picker for clipboard history.
//!
//! Same shape and keys as munin's history picker: a result list above a
//! prompt, type → re-runs the fuzzy search on every keystroke, Up/Down move
//! the selection, the full Emacs/readline editing set in the prompt. What's
//! different is what *selecting* does — these are clipboard entries, not shell
//! commands:
//!
//! - **Enter** — copy the whole entry back onto the clipboard.
//! - **Tab** — print the entry's content to stdout (pipe-friendly).
//! - **Ctrl-O** — choose one MIME of the entry, then copy just that.
//! - **Ctrl-X** — delete the entry from history (after a `y`/`n` confirm).
//! - **Esc / Ctrl-C / Ctrl-G** — cancel.
//!
//! Reads (search + preview) go straight against SQLite (WAL makes that safe
//! alongside the daemon's writer) so the picker works even when `hugind` is
//! down. The two writes — copy and delete — can only be done by the daemon
//! (it owns the wayland selection and the storage thread), so they round-trip
//! through `client::Client`; if the daemon is down they fail gracefully into
//! the status line.

use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
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
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use rusqlite::Connection;

use crate::client::Client;
use crate::config::{Config, Layout};
use crate::proto::{EntryMeta, Request, SearchSort};
use crate::{human_size, storage};

/// How much of the selected entry's text to pull for the preview pane.
const PREVIEW_CHARS: usize = 10_000;

/// What the user did. The bin layer maps each variant to an exit code and,
/// for `Print`, fetches and writes the content after the terminal is restored.
pub enum Outcome {
    /// Enter / Ctrl-O: the entry was copied to the clipboard. Bin exits 0.
    Copied,
    /// Tab: print this entry's content to stdout. Bin reads the blob and
    /// writes it once the alternate screen is torn down, then exits 0.
    Print(i64),
    /// Esc / Ctrl-C / Ctrl-G: do nothing. Bin exits 1.
    Cancel,
}

/// Cached preview body for the selected entry.
enum Preview {
    /// Leading text of a text entry.
    Text(String),
    /// Metadata block for an image / binary entry (no indexable text).
    Binary(String),
}

/// Modal sub-states layered over the normal picker.
enum Mode {
    Normal,
    /// Ctrl-X armed: waiting for `y` / `n`.
    ConfirmDelete,
    /// Ctrl-O: choosing which MIME of the selected entry to copy.
    MimeChooser {
        mimes: Vec<String>,
        sel: usize,
    },
}

/// Run the picker against the SQLite file at `db_path`. `socket_path` is the
/// daemon socket used for the copy / delete actions.
pub fn run(
    db_path: &Path,
    socket_path: &Path,
    initial_query: String,
    selection: Option<String>,
    cfg: &Config,
) -> Result<Outcome> {
    let conn =
        Connection::open(db_path).with_context(|| format!("open db {}", db_path.display()))?;

    let mut term = setup_terminal()?;
    let result = run_loop(&mut term, &conn, socket_path, initial_query, selection, cfg);
    // Always tear down the terminal, even on error, so the user gets their
    // shell back instead of a wedged session.
    restore_terminal(&mut term).ok();
    result
}

struct State {
    query: String,
    /// Byte offset into `query`. Always on a UTF-8 char boundary; all mutating
    /// handlers maintain this invariant.
    cursor: usize,
    sort: SearchSort,
    /// Selection filter (`"regular"` / `"primary"`), or `None` for both.
    selection: Option<String>,
    results: Vec<EntryMeta>,
    list_state: ListState,
    socket_path: PathBuf,
    mode: Mode,
    /// Transient message shown in the status line (e.g. "copied", an error).
    status: Option<String>,
    /// Preview cache keyed on entry id, rebuilt when the selection moves.
    preview: Option<(i64, Preview)>,
}

impl State {
    fn select(&mut self, idx: Option<usize>) {
        self.list_state.select(idx);
    }

    fn selected_entry(&self) -> Option<&EntryMeta> {
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

    /// Copy the selected entry (optionally a single MIME) via the daemon.
    /// Returns the accept outcome on success; on failure leaves a message in
    /// the status line and returns `None` so the caller stays in the loop.
    fn do_copy(&mut self, mime: Option<String>) -> Option<Outcome> {
        let entry = self.selected_entry()?;
        let req = Request::Copy {
            id: entry.id,
            selection: self.selection.clone(),
            mime,
        };
        match Client::connect(&self.socket_path).and_then(|mut c| c.request_ok(&req)) {
            Ok(()) => Some(Outcome::Copied),
            Err(e) => {
                self.status = Some(format!("copy failed: {e}"));
                None
            }
        }
    }

    /// Delete the selected entry via the daemon. On success the caller
    /// refreshes the result list; on failure the error lands in the status.
    fn do_delete(&mut self) -> bool {
        let Some(entry) = self.selected_entry() else {
            return false;
        };
        let req = Request::Delete { id: entry.id };
        match Client::connect(&self.socket_path).and_then(|mut c| c.request_ok(&req)) {
            Ok(()) => {
                self.status = Some("deleted".into());
                true
            }
            Err(e) => {
                self.status = Some(format!("delete failed: {e}"));
                false
            }
        }
    }
}

fn run_loop(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    conn: &Connection,
    socket_path: &Path,
    initial_query: String,
    selection: Option<String>,
    cfg: &Config,
) -> Result<Outcome> {
    let cursor = initial_query.len();
    let mut state = State {
        query: initial_query,
        cursor,
        sort: cfg.sort,
        selection,
        results: Vec::new(),
        list_state: ListState::default(),
        socket_path: socket_path.to_path_buf(),
        mode: Mode::Normal,
        status: None,
        preview: None,
    };
    refresh_results(&mut state, conn, cfg)?;
    ensure_preview(&mut state, conn)?;

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
        let outcome = match state.mode {
            Mode::Normal => handle_key(key, &mut state),
            Mode::ConfirmDelete => handle_confirm(key, &mut state),
            Mode::MimeChooser { .. } => handle_mime_chooser(key, &mut state),
        };
        match outcome {
            KeyOutcome::Continue => {}
            KeyOutcome::Refresh => refresh_results(&mut state, conn, cfg)?,
            KeyOutcome::Accept(o) => return Ok(o),
        }
        ensure_preview(&mut state, conn)?;
    }
}

enum KeyOutcome {
    /// Re-render but keep the current results.
    Continue,
    /// Re-run the query (query/sort/selection changed, or an entry deleted).
    Refresh,
    /// Exit the loop with this outcome.
    Accept(Outcome),
}

fn handle_key(key: KeyEvent, state: &mut State) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // ---- exit / accept ----------------------------------------------
        KeyCode::Esc => KeyOutcome::Accept(Outcome::Cancel),
        KeyCode::Char('c') if ctrl => KeyOutcome::Accept(Outcome::Cancel),
        KeyCode::Char('g') if ctrl => KeyOutcome::Accept(Outcome::Cancel),
        // Ctrl-D on empty: cancel (readline convention). Non-empty: delete the
        // char under the cursor (Emacs `delete-char`). NB this is the *prompt*
        // delete — removing a history entry is Ctrl-X.
        KeyCode::Char('d') if ctrl => {
            if state.query.is_empty() {
                KeyOutcome::Accept(Outcome::Cancel)
            } else {
                delete_forward_char(state)
            }
        }
        // Enter copies the whole entry; Tab prints it to stdout.
        KeyCode::Enter => match state.do_copy(None) {
            Some(o) => KeyOutcome::Accept(o),
            None => KeyOutcome::Continue,
        },
        KeyCode::Tab => match state.selected_entry() {
            Some(e) => KeyOutcome::Accept(Outcome::Print(e.id)),
            None => KeyOutcome::Continue,
        },

        // ---- entry actions ----------------------------------------------
        // Ctrl-O: pick a MIME to copy. Single-MIME entries skip the chooser.
        KeyCode::Char('o') if ctrl => {
            let mimes = state.selected_entry().map(|e| e.mimes.clone());
            match mimes {
                Some(mut mimes) if mimes.len() > 1 => {
                    mimes.sort();
                    state.mode = Mode::MimeChooser { mimes, sel: 0 };
                    KeyOutcome::Continue
                }
                Some(mimes) => {
                    // 0 or 1 MIME: copy directly (single MIME, or fall back to
                    // the whole entry when the list is somehow empty).
                    match state.do_copy(mimes.into_iter().next()) {
                        Some(o) => KeyOutcome::Accept(o),
                        None => KeyOutcome::Continue,
                    }
                }
                None => KeyOutcome::Continue,
            }
        }
        // Ctrl-X: arm a delete (confirmed with y/n).
        KeyCode::Char('x') if ctrl => {
            if state.selected_entry().is_some() {
                state.mode = Mode::ConfirmDelete;
            }
            KeyOutcome::Continue
        }

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
            state.status = None;
            state.query.insert(state.cursor, c);
            state.cursor += c.len_utf8();
            KeyOutcome::Refresh
        }
        _ => KeyOutcome::Continue,
    }
}

/// Ctrl-X confirmation: `y` deletes, anything else aborts.
fn handle_confirm(key: KeyEvent, state: &mut State) -> KeyOutcome {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            let deleted = state.do_delete();
            state.mode = Mode::Normal;
            if deleted {
                KeyOutcome::Refresh
            } else {
                KeyOutcome::Continue
            }
        }
        _ => {
            // n / Esc / Ctrl-G / any other key: abort the delete.
            state.mode = Mode::Normal;
            state.status = None;
            KeyOutcome::Continue
        }
    }
}

/// MIME chooser: Up/Down move, Enter copies the chosen MIME, Esc backs out.
fn handle_mime_chooser(key: KeyEvent, state: &mut State) -> KeyOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let len = match &state.mode {
        Mode::MimeChooser { mimes, .. } => mimes.len(),
        _ => return KeyOutcome::Continue,
    };
    match key.code {
        KeyCode::Esc => {
            state.mode = Mode::Normal;
            KeyOutcome::Continue
        }
        KeyCode::Char('c') | KeyCode::Char('g') if ctrl => {
            state.mode = Mode::Normal;
            KeyOutcome::Continue
        }
        KeyCode::Up | KeyCode::Char('p') => {
            if let Mode::MimeChooser { sel, .. } = &mut state.mode {
                *sel = sel.saturating_sub(1);
            }
            KeyOutcome::Continue
        }
        KeyCode::Down | KeyCode::Char('n') => {
            if let Mode::MimeChooser { sel, .. } = &mut state.mode {
                *sel = (*sel + 1).min(len.saturating_sub(1));
            }
            KeyOutcome::Continue
        }
        KeyCode::Enter => {
            let mime = match &state.mode {
                Mode::MimeChooser { mimes, sel } => mimes.get(*sel).cloned(),
                _ => None,
            };
            state.mode = Mode::Normal;
            match state.do_copy(mime) {
                Some(o) => KeyOutcome::Accept(o),
                None => KeyOutcome::Continue,
            }
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

fn refresh_results(state: &mut State, conn: &Connection, cfg: &Config) -> Result<()> {
    // `storage::search` handles the empty-query short-circuit (most-recent N).
    let mut results = storage::search(
        conn,
        &state.query,
        state.sort,
        cfg.limit,
        state.selection.as_deref(),
    )?;
    // fzf-style bottom layout wants the best/newest match nearest the prompt;
    // ratatui's List renders top→down, so reverse the vector.
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
    Ok(())
}

/// Rebuild the preview cache if the selected entry changed.
fn ensure_preview(state: &mut State, conn: &Connection) -> Result<()> {
    let Some(entry) = state.selected_entry() else {
        state.preview = None;
        return Ok(());
    };
    let id = entry.id;
    if state.preview.as_ref().map(|(cached, _)| *cached) == Some(id) {
        return Ok(());
    }
    let body = match storage::preview_text(conn, id, PREVIEW_CHARS)? {
        Some(text) => Preview::Text(text),
        None => {
            // No indexable text → image / binary. Show metadata.
            let entry = state.selected_entry().expect("entry present");
            Preview::Binary(format!(
                "[binary entry]\n\nsize:  {}\nmimes:\n{}",
                human_size(entry.size_bytes),
                entry
                    .mimes
                    .iter()
                    .map(|m| format!("  - {m}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        }
    };
    state.preview = Some((id, body));
    Ok(())
}

fn render(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config) {
    let area = f.area();
    // Split off a right-side preview pane when enabled and there's room.
    let (main_area, preview_area) = if cfg.preview && area.width > 50 {
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

    match &state.mode {
        Mode::MimeChooser { mimes, sel } => render_mime_chooser(f, cfg, mimes, *sel, list_area),
        _ => render_list(f, state, cfg, list_area),
    }
    render_status(f, state, cfg, status_area);
    render_prompt(f, state, cfg, prompt_area);
    if let Some(pa) = preview_area {
        render_preview(f, state, cfg, pa);
    }
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

    // Bottom layout: keep items flush against the prompt (best match one row
    // above it) by sub-splitting when the list doesn't fill the area.
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

/// One row: `<sel>  <size>  <snippet | [mimes]>`. Text entries show the
/// snippet (with `‹match›` highlights for searches); image/binary entries
/// show a bracketed MIME label since they have no searchable text.
fn render_row(entry: &EntryMeta, match_fg: ratatui::style::Color) -> Line<'static> {
    let sel = if entry.selection == "primary" {
        "P"
    } else {
        "C"
    };
    let size = human_size(entry.size_bytes);
    let mut spans = vec![Span::raw(format!("{sel} {size:>7}  "))];
    if let Some(snippet) = entry.snippet.as_deref() {
        spans.extend(highlight_snippet(snippet, match_fg));
    } else {
        spans.push(Span::styled(
            format!("[{}]", entry.mimes.join(", ")),
            Style::default().add_modifier(Modifier::DIM),
        ));
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

fn render_mime_chooser(
    f: &mut ratatui::Frame<'_>,
    cfg: &Config,
    mimes: &[String],
    sel: usize,
    area: Rect,
) {
    let items: Vec<ListItem> = mimes
        .iter()
        .map(|m| ListItem::new(Line::from(Span::raw(m.clone()))))
        .collect();
    let mut ls = ListState::default();
    ls.select(Some(sel.min(mimes.len().saturating_sub(1))));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(cfg.colors.selection_fg.to_ratatui())
                .bg(cfg.colors.selection_bg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut ls);
}

fn render_status(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let text = match &state.mode {
        Mode::ConfirmDelete => {
            let id = state.selected_entry().map(|e| e.id).unwrap_or(-1);
            format!(" delete entry #{id} from history?  (y/n)")
        }
        Mode::MimeChooser { .. } => " choose MIME  ·  Enter copy · Esc back".to_string(),
        Mode::Normal => {
            if let Some(msg) = &state.status {
                format!(" {msg}")
            } else {
                let sort = match state.sort {
                    SearchSort::Relevance => "relevance",
                    SearchSort::Recent => "recent",
                };
                format!(
                    " {n} match{plural}  [{sort}]   Enter copy · Tab stdout · ^O mime · ^X delete · ^R sort · Esc cancel",
                    n = state.results.len(),
                    plural = if state.results.len() == 1 { "" } else { "es" },
                )
            }
        }
    };
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
    // Only park the terminal cursor in the prompt when we're editing it;
    // in modal sub-states the focus is the list, not the query.
    if matches!(state.mode, Mode::Normal) && area.width > 0 {
        let chars_before = state.query[..state.cursor].chars().count() as u16;
        let cursor_x = (area.x + 2).saturating_add(chars_before);
        let max_x = area.x.saturating_add(area.width).saturating_sub(1);
        f.set_cursor_position((cursor_x.min(max_x), area.y));
    }
}

fn render_preview(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .title(" preview ")
        .style(Style::default().fg(cfg.colors.status_fg.to_ratatui()));
    let body = match &state.preview {
        Some((_, Preview::Text(t))) => t.clone(),
        Some((_, Preview::Binary(m))) => m.clone(),
        None => String::new(),
    };
    let p = Paragraph::new(body).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
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
