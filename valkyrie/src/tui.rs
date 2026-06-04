//! The live, modeless process picker.
//!
//! Ported from sleipnir's `/dev/tty` picker (event loop, fzf-style bottom
//! layout, readline query keymap, `‹›` highlight, preview pane). What's new for
//! Valkyrie: a **live rescan tick** so you watch processes change; structured
//! columns + a coloured state cell; the **hold-Ctrl-K** force-kill meter; a
//! **signal chooser** overlay; and scope/sort toggles. Letters feed the query,
//! so every action is a Ctrl/Tab/Fn key.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use nix::sys::signal::Signal;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as LayoutWidget, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::config::{self, Config, Layout};
use crate::proc::{Process, Scanner, current_uid, load_avg};
use crate::search::{Sort, rank};
use crate::signals::{self, CURATED, SendResult};
use crate::util::sanitize_display;

/// Key/timer poll granularity. Short enough that the hold-to-kill meter and the
/// rescan feel responsive.
const TICK: Duration = Duration::from_millis(50);
/// How often the process table is re-read.
const RESCAN_EVERY: Duration = Duration::from_millis(1000);
/// How long Alt-K must be held before SIGKILL fires.
const KILL_HOLD: Duration = Duration::from_millis(650);
/// If no Alt-K repeat arrives within this gap, the key was released → disarm.
const KILL_RELEASE_GAP: Duration = Duration::from_millis(200);
/// How long an action message lingers in the status line.
const STATUS_LINGER: Duration = Duration::from_secs(4);
/// Height of the always-on detail strip: 1 top rule + 4 content lines (exe,
/// cwd, facts, a calm system line). ^V collapses it to 0 to reclaim space.
const DETAIL_HEIGHT: u16 = 5;

pub fn run(initial_query: String) -> Result<()> {
    let cfg = config::load_or_default();
    let mut term = setup_terminal()?;
    let result = run_loop(&mut term, initial_query, &cfg);
    restore_terminal(&mut term).ok();
    result
}

/// The signal-chooser overlay (Tab). A tiny modal with its own query over the
/// curated set, plus type-any-signal.
struct Chooser {
    query: String,
    sel: usize,
    target_pid: i32,
    target_name: String,
}

struct State {
    query: String,
    cursor: usize,
    scanner: Scanner,
    snapshot: Vec<Process>,
    results: Vec<Process>,
    list_state: ListState,
    sort: Sort,
    scope_mine: bool,
    show_preview: bool,
    my_uid: u32,
    last_scan: Instant,
    /// `Some(started_at)` while Ctrl-K is held; drives the force-kill meter.
    kill_arm: Option<Instant>,
    kill_last: Instant,
    status: Option<(String, Instant)>,
    chooser: Option<Chooser>,
}

fn run_loop(
    term: &mut Terminal<CrosstermBackend<File>>,
    initial_query: String,
    cfg: &Config,
) -> Result<()> {
    let cursor = initial_query.len();
    let now = Instant::now();
    let mut state = State {
        query: initial_query,
        cursor,
        scanner: Scanner::default(),
        snapshot: Vec::new(),
        results: Vec::new(),
        list_state: ListState::default(),
        sort: Sort::Cpu,
        scope_mine: false,
        show_preview: cfg.preview,
        my_uid: current_uid(),
        last_scan: now,
        kill_arm: None,
        kill_last: now,
        status: None,
        chooser: None,
    };
    state.rescan(cfg);

    loop {
        term.draw(|f| render(f, &mut state, cfg))?;

        if event::poll(TICK)? {
            // Accept Repeat too: held keys (the force-kill meter) arrive as
            // repeats on terminals that report them, plain Press elsewhere.
            if let Event::Key(key) = event::read()?
                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && handle_key(key, &mut state, cfg)
            {
                return Ok(());
            }
        }

        let now = Instant::now();
        // Force-kill meter: fire on a sustained hold, disarm on release.
        if let Some(start) = state.kill_arm {
            if now.duration_since(state.kill_last) > KILL_RELEASE_GAP {
                state.kill_arm = None;
            } else if now.duration_since(start) >= KILL_HOLD {
                state.kill_arm = None;
                state.act(Signal::SIGKILL, cfg);
            }
        }
        // Live rescan.
        if now.duration_since(state.last_scan) >= RESCAN_EVERY {
            state.rescan(cfg);
        }
    }
}

impl State {
    fn selected(&self) -> Option<&Process> {
        self.list_state.selected().and_then(|i| self.results.get(i))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            self.list_state.select(None);
            return;
        }
        let len = self.results.len() as isize;
        let cur = self.list_state.selected().unwrap_or(0) as isize;
        self.list_state
            .select(Some((cur + delta).clamp(0, len - 1) as usize));
    }

    /// Re-read `/proc`, apply scope, re-rank — preserving the highlighted pid.
    fn rescan(&mut self, cfg: &Config) {
        let mut procs = self.scanner.scan();
        if self.scope_mine {
            let uid = self.my_uid;
            procs.retain(|p| p.uid == uid);
        }
        self.snapshot = procs;
        self.last_scan = Instant::now();
        self.refresh_results(cfg);
    }

    /// Re-rank the current snapshot for the query, keeping the selection on the
    /// same pid across refreshes (so a moving list doesn't yank your target).
    fn refresh_results(&mut self, cfg: &Config) {
        let keep_pid = self.selected().map(|p| p.pid);
        let mut results = rank(self.snapshot.clone(), &self.query, self.sort);
        results.truncate(cfg.limit);
        if matches!(cfg.layout, Layout::Bottom) {
            results.reverse();
        }
        self.results = results;

        let idx = keep_pid
            .and_then(|pid| self.results.iter().position(|p| p.pid == pid))
            .or_else(|| self.best_index());
        *self.list_state.offset_mut() = 0;
        self.list_state.select(idx);
    }

    /// The default selection (best match nearest the prompt).
    fn best_index(&self) -> Option<usize> {
        if self.results.is_empty() {
            None
        } else {
            Some(self.results.len() - 1) // Bottom layout; Top would be 0
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// Send `sig` to the selected process and report the outcome honestly.
    fn act(&mut self, sig: Signal, cfg: &Config) {
        let Some((pid, name)) = self.selected().map(|p| (p.pid, p.comm.clone())) else {
            return;
        };
        self.send_to(pid, &name, sig);
        // Immediate rescan so the consequence is visible without waiting a tick.
        self.rescan(cfg);
    }

    /// Toggle pause/resume on the selected process based on its current state.
    fn toggle_stop(&mut self, cfg: &Config) {
        let Some((pid, name, st)) = self.selected().map(|p| (p.pid, p.comm.clone(), p.state))
        else {
            return;
        };
        let sig = if st == 'T' {
            Signal::SIGCONT
        } else {
            Signal::SIGSTOP
        };
        self.send_to(pid, &name, sig);
        self.rescan(cfg);
    }

    fn send_to(&mut self, pid: i32, name: &str, sig: Signal) {
        let msg = match signals::send(pid, sig) {
            SendResult::Sent => format!("sent {} to {name} [{pid}]", sig.as_str()),
            SendResult::NeedRoot => format!("{name} [{pid}] needs root — can't signal"),
            SendResult::Gone => format!("{name} [{pid}] already gone"),
            SendResult::Err(e) => format!("{name} [{pid}]: {e}"),
        };
        self.set_status(msg);
    }
}

/// Returns `true` to quit.
fn handle_key(key: KeyEvent, state: &mut State, cfg: &Config) -> bool {
    if state.chooser.is_some() {
        return handle_chooser_key(key, state, cfg);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // ---- quit -------------------------------------------------------
        KeyCode::Esc => return true,
        KeyCode::Char('c') if ctrl => return true,
        KeyCode::Char('g') if ctrl => return true,

        // ---- actions ----------------------------------------------------
        KeyCode::Enter => state.act(Signal::SIGTERM, cfg),
        // Hold Alt-K (M-k) → force-kill meter. Each press (incl. auto-repeat)
        // keeps it armed; the loop fires once it's been held long enough.
        // (Ctrl-K stays Emacs kill-to-end-of-line in the query — never a process
        // action, so an editing reflex can't trigger a kill.)
        KeyCode::Char('k') if alt => {
            let now = Instant::now();
            if state.kill_arm.is_none() || now.duration_since(state.kill_last) > KILL_RELEASE_GAP {
                state.kill_arm = Some(now);
            }
            state.kill_last = now;
        }
        // Ctrl-Z: pause/resume toggle.
        KeyCode::Char('z') if ctrl => state.toggle_stop(cfg),
        // Tab: open the signal chooser for the selected process.
        KeyCode::Tab => {
            if let Some(p) = state.selected() {
                state.chooser = Some(Chooser {
                    query: String::new(),
                    sel: 0,
                    target_pid: p.pid,
                    target_name: p.comm.clone(),
                });
            }
        }

        // ---- view toggles ----------------------------------------------
        KeyCode::Char('o') if ctrl => {
            state.scope_mine = !state.scope_mine;
            state.rescan(cfg);
        }
        KeyCode::Char('r') if ctrl => {
            state.sort = state.sort.cycle();
            state.refresh_results(cfg);
        }
        KeyCode::Char('l') if ctrl => state.rescan(cfg),
        KeyCode::Char('v') if ctrl => state.show_preview = !state.show_preview,

        // ---- list navigation -------------------------------------------
        KeyCode::Up => state.move_selection(-1),
        KeyCode::Down => state.move_selection(1),
        KeyCode::Char('p') if ctrl => state.move_selection(-1),
        KeyCode::Char('n') if ctrl => state.move_selection(1),
        KeyCode::PageUp => state.move_selection(-10),
        KeyCode::PageDown => state.move_selection(10),

        // ---- query cursor / editing (readline subset) ------------------
        KeyCode::Left => state.cursor = prev_off(&state.query, state.cursor),
        KeyCode::Right => state.cursor = next_off(&state.query, state.cursor),
        KeyCode::Char('b') if ctrl => state.cursor = prev_off(&state.query, state.cursor),
        KeyCode::Char('f') if ctrl => state.cursor = next_off(&state.query, state.cursor),
        KeyCode::Char('a') if ctrl => state.cursor = 0,
        KeyCode::Char('e') if ctrl => state.cursor = state.query.len(),
        KeyCode::Home => state.cursor = 0,
        KeyCode::End => state.cursor = state.query.len(),
        KeyCode::Backspace => delete_back(state, cfg),
        KeyCode::Char('h') if ctrl => delete_back(state, cfg),
        KeyCode::Char('k') if ctrl => kill_to_end(state, cfg),
        KeyCode::Char('u') if ctrl => {
            if !state.query.is_empty() {
                state.query.clear();
                state.cursor = 0;
                state.refresh_results(cfg);
            }
        }
        KeyCode::Char('w') if ctrl => delete_word(state, cfg),

        // ---- typed char → query ----------------------------------------
        // `!alt` so Alt-combos (e.g. Alt-K force-kill) aren't typed as text.
        KeyCode::Char(c) if !ctrl && !alt => {
            state.query.insert(state.cursor, c);
            state.cursor += c.len_utf8();
            state.refresh_results(cfg);
        }
        _ => {}
    }
    false
}

fn handle_chooser_key(key: KeyEvent, state: &mut State, cfg: &Config) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some(ch) = state.chooser.as_mut() else {
        return false;
    };
    match key.code {
        KeyCode::Esc => state.chooser = None,
        KeyCode::Char('c') | KeyCode::Char('g') if ctrl => state.chooser = None,
        KeyCode::Up => ch.sel = ch.sel.saturating_sub(1),
        KeyCode::Down => ch.sel += 1,
        KeyCode::Char('p') if ctrl => ch.sel = ch.sel.saturating_sub(1),
        KeyCode::Char('n') if ctrl => ch.sel += 1,
        KeyCode::Backspace => {
            ch.query.pop();
            ch.sel = 0;
        }
        KeyCode::Enter => {
            let items = chooser_items(&ch.query);
            if let Some((sig, _)) = items.get(ch.sel.min(items.len().saturating_sub(1))) {
                let (sig, pid, name) = (*sig, ch.target_pid, ch.target_name.clone());
                state.chooser = None;
                state.send_to(pid, &name, sig);
                state.rescan(cfg);
            } else {
                state.chooser = None;
            }
        }
        KeyCode::Char(c) if !ctrl => {
            ch.query.push(c);
            ch.sel = 0;
        }
        _ => {}
    }
    false
}

/// The chooser's current filtered items: curated signals matching the query,
/// plus a type-any entry when the query parses to a signal not already listed.
fn chooser_items(query: &str) -> Vec<(Signal, String)> {
    let q = query.trim().to_ascii_lowercase();
    let mut v: Vec<(Signal, String)> = CURATED
        .iter()
        .filter(|s| {
            q.is_empty() || s.sig.as_str().to_ascii_lowercase().contains(&q) || s.desc.contains(&q)
        })
        .map(|s| (s.sig, format!("{:<8} {}", s.sig.as_str(), s.desc)))
        .collect();
    if let Some(sig) = signals::parse_signal(query)
        && !v.iter().any(|(s, _)| *s == sig)
    {
        v.insert(0, (sig, format!("{:<8} (typed)", sig.as_str())));
    }
    v
}

// ---- query editing helpers (from sleipnir) --------------------------------

fn prev_off(s: &str, pos: usize) -> usize {
    s[..pos]
        .chars()
        .next_back()
        .map_or(0, |c| pos - c.len_utf8())
}
fn next_off(s: &str, pos: usize) -> usize {
    s[pos..]
        .chars()
        .next()
        .map_or(s.len(), |c| pos + c.len_utf8())
}
fn delete_back(state: &mut State, cfg: &Config) {
    if state.cursor == 0 {
        return;
    }
    let prev = prev_off(&state.query, state.cursor);
    state.query.replace_range(prev..state.cursor, "");
    state.cursor = prev;
    state.refresh_results(cfg);
}
fn kill_to_end(state: &mut State, cfg: &Config) {
    if state.cursor >= state.query.len() {
        return;
    }
    state.query.truncate(state.cursor);
    state.refresh_results(cfg);
}
fn delete_word(state: &mut State, cfg: &Config) {
    if state.cursor == 0 {
        return;
    }
    let trimmed = state.query[..state.cursor].trim_end();
    let start = trimmed
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    state.query.replace_range(start..state.cursor, "");
    state.cursor = start;
    state.refresh_results(cfg);
}

// ---- rendering ------------------------------------------------------------

fn render(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config) {
    let area = f.area();
    // The detail strip sits just above the prompt (Bottom layout) — adjacent to
    // the selected row and your typing, where your attention already is (Raskin:
    // minimise locus-of-attention travel; not htop's top header). ^V collapses
    // it to height 0.
    let detail_h = if state.show_preview { DETAIL_HEIGHT } else { 0 };
    let chunks = LayoutWidget::default()
        .direction(Direction::Vertical)
        .constraints(match cfg.layout {
            Layout::Bottom => [
                Constraint::Min(1),
                Constraint::Length(detail_h),
                Constraint::Length(1),
                Constraint::Length(1),
            ],
            Layout::Top => [
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(detail_h),
                Constraint::Min(1),
            ],
        })
        .split(area);
    let (list_area, detail_area, status_area, prompt_area) = match cfg.layout {
        Layout::Bottom => (chunks[0], chunks[1], chunks[2], chunks[3]),
        Layout::Top => (chunks[3], chunks[2], chunks[1], chunks[0]),
    };

    render_list(f, state, cfg, list_area);
    if detail_h > 0 {
        render_detail(f, state, cfg, detail_area);
    }
    render_status(f, state, cfg, status_area);
    render_prompt(f, state, cfg, prompt_area);
    if state.chooser.is_some() {
        render_chooser(f, state, cfg, area);
    }
}

fn render_list(f: &mut ratatui::Frame<'_>, state: &mut State, cfg: &Config, area: Rect) {
    let match_fg = cfg.colors.match_fg.to_ratatui();
    let label_fg = cfg.colors.status_fg.to_ratatui();
    let items: Vec<ListItem> = state
        .results
        .iter()
        .map(|p| ListItem::new(render_row(p, match_fg, label_fg)))
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

    let list_area = match cfg.layout {
        Layout::Bottom if item_count < area.height => {
            let c = LayoutWidget::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(item_count)])
                .split(area);
            c[1]
        }
        _ => area,
    };
    f.render_stateful_widget(list, list_area, &mut state.list_state);
}

/// `  PID USER   %CPU    RSS S command` — columns in the label colour, the
/// state cell coloured by state, the command highlighted on a match.
fn render_row(p: &Process, match_fg: Color, label_fg: Color) -> Line<'static> {
    let cols = format!(
        "{:>7} {:<10} {:>5.1} {:>8} ",
        p.pid,
        truncate(&p.user, 10),
        p.cpu_pct,
        human_kb(p.rss_kb),
    );
    let mut spans = vec![
        Span::styled(cols, Style::default().fg(label_fg)),
        Span::styled(
            format!("{} ", p.state),
            Style::default().fg(state_color(p.state)),
        ),
    ];
    match p.snippet.as_deref() {
        Some(snip) => spans.extend(highlight_snippet(snip, match_fg)),
        None => spans.push(Span::raw(sanitize_display(&p.command))),
    }
    Line::from(spans)
}

fn highlight_snippet(s: &str, match_fg: Color) -> Vec<Span<'static>> {
    let s = sanitize_display(s);
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut in_match = false;
    let matched = Style::default().fg(match_fg).add_modifier(Modifier::BOLD);
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
                    spans.push(Span::styled(std::mem::take(&mut buf), matched));
                }
                in_match = false;
            }
            c => buf.push(c),
        }
    }
    if !buf.is_empty() {
        if in_match {
            spans.push(Span::styled(buf, matched));
        } else {
            spans.push(Span::raw(buf));
        }
    }
    spans
}

fn render_status(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let warn = Style::default().fg(cfg.colors.warn_fg.to_ratatui());
    let dim = Style::default().fg(cfg.colors.status_fg.to_ratatui());

    // Priority: force-kill meter > recent action message > the hint line.
    let line = if let Some(start) = state.kill_arm {
        let name = state.selected().map(|p| p.comm.as_str()).unwrap_or("?");
        let frac = start.elapsed().as_secs_f32() / KILL_HOLD.as_secs_f32();
        Line::from(Span::styled(
            format!(" hold M-k to force-kill {name}  {}", meter(frac)),
            warn.add_modifier(Modifier::BOLD),
        ))
    } else if let Some((msg, at)) = &state.status {
        if at.elapsed() < STATUS_LINGER {
            Line::from(Span::styled(format!(" {msg}"), warn))
        } else {
            hint_line(state, dim)
        }
    } else {
        hint_line(state, dim)
    };
    f.render_widget(Paragraph::new(line), area);
}

fn hint_line(state: &State, dim: Style) -> Line<'static> {
    let scope = if state.scope_mine { "mine" } else { "all" };
    Line::from(Span::styled(
        format!(
            " {n} procs · sort:{sort} · scope:{scope} · Enter term · ^Z stop · hold M-k kill · Tab signal · ^O scope · ^R sort · ^V detail · Esc",
            n = state.results.len(),
            sort = state.sort.label(),
        ),
        dim,
    ))
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

/// The horizontal detail strip (above the prompt): the selected process's focal
/// facts — exe, cwd, parent, threads, state, ports — plus one calm system line.
/// Knowledge at the locus of attention; best-effort, unreadable bits show "—".
fn render_detail(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let dim = Style::default().fg(cfg.colors.status_fg.to_ratatui());
    let block = Block::default().borders(Borders::TOP).border_style(dim);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(p) = state.selected() {
        let exe = read_link(p.pid, "exe").unwrap_or_else(|| "—".into());
        let cwd = read_link(p.pid, "cwd").unwrap_or_else(|| "—".into());
        let parent = parent_comm(p.ppid, &state.snapshot);
        let ports = if p.ports.is_empty() {
            "—".into()
        } else {
            p.ports
                .iter()
                .map(|n| format!(":{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        lines.push(pair_line(&[("exe", exe)], dim));
        lines.push(pair_line(&[("cwd", cwd)], dim));
        lines.push(pair_line(
            &[
                ("ppid", format!("{} {}", p.ppid, parent)),
                ("thr", p.threads.to_string()),
                ("state", format!("{} {}", p.state, state_desc(p.state))),
                ("ports", ports),
            ],
            dim,
        ));
    } else {
        lines.push(Line::from(Span::styled("no process selected", dim)));
        lines.push(Line::raw(""));
        lines.push(Line::raw(""));
    }
    lines.push(system_line(state, dim));
    f.render_widget(Paragraph::new(lines), inner);
}

/// One line of `label value` pairs separated by `·`, labels dimmed.
fn pair_line(pairs: &[(&str, String)], label: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", label));
        }
        spans.push(Span::styled(format!("{k} "), label));
        spans.push(Span::raw(sanitize_display(v)));
    }
    Line::from(spans)
}

/// The calm aggregate: overall CPU as a single bar that only warms to
/// yellow/red when genuinely busy (it never cries wolf), plus load average and
/// task counts.
fn system_line(state: &State, dim: Style) -> Line<'static> {
    let cpu = state.scanner.system_cpu();
    let (a, b, c) = load_avg();
    let procs = state.snapshot.len();
    let running = state.snapshot.iter().filter(|p| p.state == 'R').count();
    let bar_color = if cpu < 70.0 {
        Color::Green
    } else if cpu < 90.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    Line::from(vec![
        Span::styled("CPU ", dim),
        Span::styled(bar(cpu / 100.0, 12), Style::default().fg(bar_color)),
        Span::styled(format!(" {cpu:>3.0}%    "), dim),
        Span::styled("load ", dim),
        Span::raw(format!("{a:.2} {b:.2} {c:.2}")),
        Span::styled(format!("    {procs} procs · {running} running"), dim),
    ])
}

fn bar(frac: f32, cells: usize) -> String {
    let filled = (frac.clamp(0.0, 1.0) * cells as f32).round() as usize;
    (0..cells)
        .map(|i| if i < filled { '▰' } else { '▱' })
        .collect()
}

fn parent_comm(ppid: i32, snapshot: &[Process]) -> String {
    snapshot
        .iter()
        .find(|p| p.pid == ppid)
        .map(|p| p.comm.clone())
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{ppid}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "?".into())
}

fn render_chooser(f: &mut ratatui::Frame<'_>, state: &State, cfg: &Config, area: Rect) {
    let Some(ch) = state.chooser.as_ref() else {
        return;
    };
    let items = chooser_items(&ch.query);
    let sel = ch.sel.min(items.len().saturating_sub(1));
    let w = 52u16.min(area.width.saturating_sub(2));
    let h = (items.len() as u16 + 4).min(area.height.saturating_sub(2));
    let rect = centered(area, w, h);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(cfg.colors.prompt_fg.to_ratatui()))
        .title(format!(" signal → {} [{}] ", ch.target_name, ch.target_pid));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let chunks = LayoutWidget::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(cfg.colors.prompt_fg.to_ratatui())),
            Span::raw(ch.query.clone()),
        ])),
        chunks[0],
    );
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|(_, label)| ListItem::new(Line::raw(label.clone())))
        .collect();
    let mut ls = ListState::default();
    if !items.is_empty() {
        ls.select(Some(sel));
    }
    let list = List::new(list_items)
        .highlight_style(
            Style::default()
                .fg(cfg.colors.selection_fg.to_ratatui())
                .bg(cfg.colors.selection_bg.to_ratatui())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[1], &mut ls);
}

// ---- small helpers --------------------------------------------------------

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn meter(frac: f32) -> String {
    let n = 5;
    let filled = (frac.clamp(0.0, 1.0) * n as f32).round() as usize;
    let mut s = String::new();
    for i in 0..n {
        s.push(if i < filled { '▰' } else { '▱' });
    }
    s
}

fn human_kb(kb: u64) -> String {
    if kb < 1024 {
        format!("{kb}K")
    } else if kb < 1024 * 1024 {
        format!("{:.1}M", kb as f64 / 1024.0)
    } else {
        format!("{:.1}G", kb as f64 / (1024.0 * 1024.0))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn state_color(state: char) -> Color {
    match state {
        'R' => Color::Green,
        'D' => Color::Red,
        'Z' => Color::Magenta,
        'T' | 't' => Color::Yellow,
        _ => Color::Gray,
    }
}

fn state_desc(state: char) -> &'static str {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "uninterruptible (won't die even on SIGKILL)",
        'T' => "stopped",
        't' => "traced",
        'Z' => "zombie",
        'I' => "idle",
        _ => "",
    }
}

fn read_link(pid: i32, what: &str) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/{what}"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

// ---- terminal setup (sleipnir's /dev/tty trick) ---------------------------

fn setup_terminal() -> Result<Terminal<CrosstermBackend<File>>> {
    let mut tty = File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("open /dev/tty")?;
    enable_raw_mode().context("enable raw mode")?;
    execute!(tty, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let _ = tty.flush();
    let backend = CrosstermBackend::new(tty);
    Terminal::new(backend).context("create terminal")
}

fn restore_terminal(term: &mut Terminal<CrosstermBackend<File>>) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_fills_with_fraction() {
        assert_eq!(meter(0.0), "▱▱▱▱▱");
        assert_eq!(meter(1.0), "▰▰▰▰▰");
        assert_eq!(meter(0.5).chars().filter(|&c| c == '▰').count(), 3); // 0.5*5≈3
    }

    #[test]
    fn human_kb_buckets() {
        assert_eq!(human_kb(512), "512K");
        assert_eq!(human_kb(2048), "2.0M");
    }

    #[test]
    fn chooser_includes_typed_signal() {
        // Typing a signal not in the curated-visible filter still offers it.
        let items = chooser_items("usr1");
        assert!(items.iter().any(|(s, _)| *s == Signal::SIGUSR1));
    }
}
