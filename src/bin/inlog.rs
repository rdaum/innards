#[path = "navsplat/clipboard.rs"]
mod clipboard;

use std::env;
use std::io::{self, Stdout, Write};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::ExecutableCommand;
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};
use innards::config::{InnardsConfig, KeyPress, Keymap, KeymapMatch};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

const DEFAULT_HEIGHT: u16 = 20;
const MIN_HEIGHT: u16 = 6;
const DEFAULT_MAX_COUNT: &str = "500";
const RECORD_SEP: char = '\x1e';
const FIELD_SEP: char = '\x1f';
const GIT_LOG_FORMAT: &str = "%x1e%H%x1f%h%x1f%an%x1f%ae%x1f%as%x1f%aI%x1f%cI%x1f%D%x1f%s%x1f%b";

const LOG_KEY_BINDINGS: &[(&str, &[&str])] = &[
    ("quit", &["ctrl-x ctrl-c", "esc", "q"]),
    ("accept", &["enter"]),
    ("search_forward", &["ctrl-s"]),
    ("search_reverse", &["ctrl-r"]),
    ("cancel_search", &["esc", "ctrl-g"]),
    ("finish_search", &["enter"]),
    ("search_backspace", &["backspace"]),
    ("select_prev", &["ctrl-p", "up"]),
    ("select_next", &["ctrl-n", "down"]),
    ("page_up", &["pageup", "alt-v"]),
    ("page_down", &["pagedown", "ctrl-v"]),
    ("shrink_height", &["alt-up"]),
    ("grow_height", &["alt-down"]),
    ("fullscreen", &["ctrl-x 1"]),
    ("restore_inline", &["ctrl-x 0"]),
    ("toggle_expand", &["/"]),
    ("copy", &["alt-y"]),
];

const LOG_ACTIONS: &[&str] = &[
    "quit",
    "accept",
    "search_forward",
    "search_reverse",
    "select_prev",
    "select_next",
    "page_up",
    "page_down",
    "shrink_height",
    "grow_height",
    "fullscreen",
    "restore_inline",
    "toggle_expand",
    "copy",
];
const LOG_SEARCH_ACTIONS: &[&str] = &[
    "quit",
    "fullscreen",
    "restore_inline",
    "search_forward",
    "search_reverse",
    "cancel_search",
    "finish_search",
    "search_backspace",
];

fn main() -> Result<()> {
    let config = Config::parse(env::args().skip(1))?;
    let innards_config = InnardsConfig::load()?;
    let mut keymap = Keymap::from_defaults(LOG_KEY_BINDINGS)?;
    keymap.apply_overrides(&innards_config.keybindings.log)?;

    let commits = load_commits(&config.git_args)?;
    let mut app = App::new(commits, config.height);
    let mut terminal = TerminalGuard::enter(config.height)?;
    run_event_loop(&mut terminal, &mut app, &keymap)?;
    let selected = app.selected_commit().cloned();
    let accepted = app.accepted;
    let summary = app
        .selected_commit()
        .map(|commit| commit.summary_text(true))
        .unwrap_or_else(|| "no commits\n".to_string());
    if app.fullscreen {
        terminal.leave_fullscreen(app.restore_height.unwrap_or(app.height))?;
        app.fullscreen = false;
    }
    terminal.collapse(app.last_drawn_top)?;
    drop(terminal);
    print!("{summary}");
    if accepted {
        print!("{}", exit_copy_status(selected.as_ref()));
    }
    io::stdout().flush()?;
    Ok(())
}

struct Config {
    height: u16,
    git_args: Vec<String>,
}

impl Config {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut height = DEFAULT_HEIGHT;
        let mut git_args = Vec::new();
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            if arg == "--height" || arg == "-h" {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("{arg} requires a row count"))?;
                height = value
                    .parse::<u16>()
                    .with_context(|| format!("invalid height: {value}"))?;
            } else if let Some(value) = arg.strip_prefix("--height=") {
                height = value
                    .parse::<u16>()
                    .with_context(|| format!("invalid height: {value}"))?;
            } else if arg == "--" {
                git_args.extend(args);
                break;
            } else {
                git_args.push(arg);
            }
        }

        normalize_git_args(git_args).map(|git_args| Self { height, git_args })
    }
}

fn normalize_git_args(mut args: Vec<String>) -> Result<Vec<String>> {
    if args
        .iter()
        .any(|arg| arg == "--graph" || arg.starts_with("--graph="))
    {
        return Err(anyhow!("inlog does not support --graph"));
    }
    if !has_max_count_arg(&args) {
        args.splice(0..0, ["-n".to_string(), DEFAULT_MAX_COUNT.to_string()]);
    }
    Ok(args)
}

fn has_max_count_arg(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "--max-count"
            || arg.starts_with("--max-count=")
            || arg == "-n"
            || arg
                .strip_prefix("-n")
                .is_some_and(|value| !value.is_empty())
            || (arg.starts_with('-') && arg[1..].chars().all(|ch| ch.is_ascii_digit()))
    })
}

fn load_commits(git_args: &[String]) -> Result<Vec<Commit>> {
    let mut command = Command::new("git");
    command
        .arg("log")
        .arg("--date=short")
        .arg(format!("--format=format:{GIT_LOG_FORMAT}"))
        .args(git_args);
    let output = command.output().context("failed to run git log")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git log failed: {}", stderr.trim()));
    }
    Commit::parse_many(&String::from_utf8_lossy(&output.stdout))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Commit {
    full_hash: String,
    short_hash: String,
    author_name: String,
    author_email: String,
    author_date: String,
    author_date_full: String,
    commit_date_full: String,
    refs: String,
    subject: String,
    body: String,
}

impl Commit {
    fn parse_many(source: &str) -> Result<Vec<Self>> {
        source
            .split(RECORD_SEP)
            .filter(|record| !record.trim().is_empty())
            .map(Self::parse_record)
            .collect()
    }

    fn parse_record(record: &str) -> Result<Self> {
        let mut fields = record.trim_start_matches('\n').splitn(10, FIELD_SEP);
        let next = |fields: &mut std::str::SplitN<'_, char>| {
            fields
                .next()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("malformed git log record"))
        };
        Ok(Self {
            full_hash: next(&mut fields)?,
            short_hash: next(&mut fields)?,
            author_name: next(&mut fields)?,
            author_email: next(&mut fields)?,
            author_date: next(&mut fields)?,
            author_date_full: next(&mut fields)?,
            commit_date_full: next(&mut fields)?,
            refs: next(&mut fields)?,
            subject: next(&mut fields)?,
            body: fields.next().unwrap_or_default().trim_end().to_string(),
        })
    }

    fn searchable_text(&self) -> String {
        format!(
            "{} {} {} {} {} {} {} {} {} {}",
            self.full_hash,
            self.short_hash,
            self.author_name,
            self.author_email,
            self.author_date,
            self.author_date_full,
            self.commit_date_full,
            self.refs,
            self.subject,
            self.body
        )
        .to_ascii_lowercase()
    }

    fn summary_text(&self, color: bool) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} {}    {} {}\n",
            paint_label("commit", color),
            paint_value(&self.full_hash, AnsiColor::Cyan, color),
            paint_label("author", color),
            paint_value(
                &format!("{} <{}>", self.author_name, self.author_email),
                AnsiColor::Green,
                color
            )
        ));
        out.push_str(&format!(
            "{} {}    {} {}\n",
            paint_label("author date", color),
            paint_value(&self.author_date_full, AnsiColor::Yellow, color),
            paint_label("commit date", color),
            paint_value(&self.commit_date_full, AnsiColor::Magenta, color)
        ));
        if !self.refs.is_empty() {
            out.push_str(&format!(
                "{} {}\n",
                paint_label("refs", color),
                paint_value(&self.refs, AnsiColor::Blue, color)
            ));
        }
        out.push('\n');
        out.push_str(&format!(
            "{} {}\n",
            paint_label("subject", color),
            paint_value(
                &truncate_with_dots(&self.subject, 50),
                AnsiColor::Bold,
                color
            )
        ));
        out
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AnsiColor {
    Blue,
    Bold,
    Cyan,
    Green,
    Magenta,
    Yellow,
}

fn paint_label(text: &str, color: bool) -> String {
    if color {
        format!("\x1b[90m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn paint_value(text: &str, ansi: AnsiColor, color: bool) -> String {
    if !color {
        return text.to_string();
    }
    let code = match ansi {
        AnsiColor::Blue => 34,
        AnsiColor::Bold => 1,
        AnsiColor::Cyan => 36,
        AnsiColor::Green => 32,
        AnsiColor::Magenta => 35,
        AnsiColor::Yellow => 33,
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchDirection {
    Forward,
    Reverse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchState {
    query: String,
    direction: SearchDirection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct App {
    commits: Vec<Commit>,
    selected: usize,
    /// Top visible rendered row. Expanded commits add rendered rows, so this is
    /// deliberately not a commit index.
    scroll_y: usize,
    expanded: Option<usize>,
    status: String,
    pending_keys: Vec<KeyPress>,
    search: Option<SearchState>,
    visible_height: usize,
    height: u16,
    restore_height: Option<u16>,
    fullscreen: bool,
    last_drawn_height: u16,
    last_drawn_top: u16,
    accepted: bool,
}

impl App {
    fn new(commits: Vec<Commit>, height: u16) -> Self {
        let status = if commits.is_empty() {
            "no commits".to_string()
        } else {
            "Ctrl-S search  / expand  Alt-Y copy SHA".to_string()
        };
        Self {
            commits,
            selected: 0,
            scroll_y: 0,
            expanded: None,
            status,
            pending_keys: Vec::new(),
            search: None,
            visible_height: height.max(MIN_HEIGHT) as usize,
            height: height.max(MIN_HEIGHT),
            restore_height: None,
            fullscreen: false,
            last_drawn_height: height.max(MIN_HEIGHT),
            last_drawn_top: 0,
            accepted: false,
        }
    }

    fn selected_commit(&self) -> Option<&Commit> {
        self.commits.get(self.selected)
    }

    fn select_delta(&mut self, delta: isize) {
        if self.commits.is_empty() {
            return;
        }
        let max = self.commits.len() - 1;
        if delta < 0 {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected = (self.selected + delta as usize).min(max);
        }
    }

    fn page_amount(&self, height: usize) -> isize {
        height.saturating_sub(2).max(1) as isize
    }

    fn visible_page_amount(&self) -> isize {
        self.page_amount(self.visible_height)
    }

    fn toggle_expand(&mut self) {
        self.expanded = if self.expanded == Some(self.selected) {
            None
        } else {
            Some(self.selected)
        };
    }

    fn accept(&mut self) {
        self.accepted = true;
    }

    fn begin_search(&mut self, direction: SearchDirection) {
        self.search = Some(SearchState {
            query: String::new(),
            direction,
        });
        self.update_search_status();
    }

    fn cancel_search(&mut self) {
        if self.search.take().is_some() {
            self.status = "search cancelled".to_string();
        }
    }

    fn finish_search(&mut self) {
        if self.search.take().is_some() {
            self.status = "search done".to_string();
        }
    }

    fn search_insert_char(&mut self, ch: char) {
        if let Some(search) = self.search.as_mut() {
            search.query.push(ch);
        }
        self.apply_search(false);
    }

    fn search_backspace(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.query.pop();
        }
        self.apply_search(false);
    }

    fn search_repeat(&mut self, direction: SearchDirection) {
        if let Some(search) = self.search.as_mut() {
            search.direction = direction;
        }
        self.apply_search(true);
    }

    fn apply_search(&mut self, repeated: bool) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        if search.query.is_empty() || self.commits.is_empty() {
            self.update_search_status();
            return;
        }
        let query = search.query.to_ascii_lowercase();
        let start = if repeated {
            match search.direction {
                SearchDirection::Forward => self.selected.saturating_add(1),
                SearchDirection::Reverse => self.selected.saturating_sub(1),
            }
        } else {
            self.selected
        };
        if let Some(idx) = self.find_match(search.direction, &query, start) {
            self.selected = idx;
        }
        self.update_search_status();
    }

    fn find_match(&self, direction: SearchDirection, query: &str, start: usize) -> Option<usize> {
        if self.commits.is_empty() {
            return None;
        }
        match direction {
            SearchDirection::Forward => (start..self.commits.len())
                .chain(0..start.min(self.commits.len()))
                .find(|&idx| self.commits[idx].searchable_text().contains(query)),
            SearchDirection::Reverse => (0..=start.min(self.commits.len() - 1))
                .rev()
                .chain(((start + 1).min(self.commits.len())..self.commits.len()).rev())
                .find(|&idx| self.commits[idx].searchable_text().contains(query)),
        }
    }

    fn update_search_status(&mut self) {
        if let Some(search) = self.search.as_ref() {
            let label = match search.direction {
                SearchDirection::Forward => "I-search",
                SearchDirection::Reverse => "I-search backward",
            };
            self.status = format!("{label}: {}", search.query);
        }
    }

    fn copy_selected_hash(&mut self) {
        let Some(hash) = self
            .selected_commit()
            .map(|commit| commit.full_hash.clone())
        else {
            return;
        };
        match clipboard::copy_to_clipboard(&hash) {
            Ok(method) => self.status = format!("copied SHA via {method}"),
            Err(err) => self.status = format!("copy failed: {err}"),
        }
    }

    fn ensure_selection_visible(&mut self, height: usize) {
        let selected_row = self.row_offset_for_commit(self.selected);
        if selected_row < self.scroll_y {
            self.scroll_y = selected_row;
        } else if selected_row >= self.scroll_y.saturating_add(height) {
            self.scroll_y = self.commit_aligned_scroll_offset(
                selected_row.saturating_sub(height.saturating_sub(1)),
                selected_row,
            );
        }
    }

    fn row_offset_for_commit(&self, commit_idx: usize) -> usize {
        (0..commit_idx.min(self.commits.len()))
            .map(|idx| self.commit_render_height(idx))
            .sum()
    }

    fn commit_render_height(&self, commit_idx: usize) -> usize {
        let Some(commit) = self.commits.get(commit_idx) else {
            return 0;
        };
        1 + if self.expanded == Some(commit_idx) {
            expanded_commit_line_count(commit)
        } else {
            0
        }
    }

    fn commit_aligned_scroll_offset(&self, desired: usize, selected_row: usize) -> usize {
        let mut offset = 0usize;
        for idx in 0..self.commits.len() {
            let height = self.commit_render_height(idx);
            let next_offset = offset.saturating_add(height);
            if desired <= offset {
                return offset.min(selected_row);
            }
            if desired < next_offset {
                return next_offset.min(selected_row);
            }
            offset = next_offset;
        }
        selected_row
    }
}

fn run_event_loop(terminal: &mut TerminalGuard, app: &mut App, keymap: &Keymap) -> Result<()> {
    loop {
        terminal.terminal.draw(|frame| draw(frame, app))?;
        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && handle_key(app, key, terminal, keymap)?
        {
            return Ok(());
        }
    }
}

fn handle_key(
    app: &mut App,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
    keymap: &Keymap,
) -> Result<bool> {
    if app.search.is_some() {
        if handle_key_binding(app, key, terminal, keymap)? {
            return Ok(true);
        }
        if let KeyCode::Char(ch) = key.code
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            app.search_insert_char(ch);
        }
        return Ok(false);
    }
    handle_key_binding(app, key, terminal, keymap)
}

fn handle_key_binding(
    app: &mut App,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
    keymap: &Keymap,
) -> Result<bool> {
    let actions = if app.search.is_some() {
        LOG_SEARCH_ACTIONS
    } else {
        LOG_ACTIONS
    };
    match keymap.match_key_for_actions(actions, &app.pending_keys, &key) {
        KeymapMatch::Prefix => {
            if let Some(key) = keymap.keypress_from_event(&key) {
                app.pending_keys.push(key);
                app.status = pending_status(&app.pending_keys);
            }
            Ok(false)
        }
        KeymapMatch::Action(action) => {
            app.pending_keys.clear();
            handle_action(app, terminal, action.as_str())
        }
        KeymapMatch::None if !app.pending_keys.is_empty() => {
            app.pending_keys.clear();
            app.status = "unknown key sequence".to_string();
            Ok(false)
        }
        KeymapMatch::None => Ok(false),
    }
}

fn handle_action(app: &mut App, terminal: &mut TerminalGuard, action: &str) -> Result<bool> {
    match action {
        "quit" => return Ok(true),
        "accept" => {
            app.accept();
            return Ok(true);
        }
        "search_forward" if app.search.is_some() => app.search_repeat(SearchDirection::Forward),
        "search_forward" => app.begin_search(SearchDirection::Forward),
        "search_reverse" if app.search.is_some() => app.search_repeat(SearchDirection::Reverse),
        "search_reverse" => app.begin_search(SearchDirection::Reverse),
        "cancel_search" => app.cancel_search(),
        "finish_search" => app.finish_search(),
        "search_backspace" => app.search_backspace(),
        "select_prev" => app.select_delta(-1),
        "select_next" => app.select_delta(1),
        "page_up" => app.select_delta(-app.visible_page_amount()),
        "page_down" => app.select_delta(app.visible_page_amount()),
        "shrink_height" => resize_inline_log(app, terminal, app.height.saturating_sub(1))?,
        "grow_height" => resize_inline_log(app, terminal, app.height.saturating_add(1))?,
        "fullscreen" => fullscreen_log(app, terminal)?,
        "restore_inline" => restore_inline_log(app, terminal)?,
        "toggle_expand" => app.toggle_expand(),
        "copy" => app.copy_selected_hash(),
        _ => {}
    }
    Ok(false)
}

fn exit_copy_status(commit: Option<&Commit>) -> String {
    let Some(commit) = commit else {
        return String::new();
    };
    match clipboard::copy_to_clipboard(&commit.full_hash) {
        Ok(method) => format!(
            "\n{} {}\n",
            paint_label("clipboard", true),
            paint_value(
                &format!("copied {} via {method}", commit.full_hash),
                AnsiColor::Green,
                true
            )
        ),
        Err(err) => format!(
            "\n{} {}\n",
            paint_label("clipboard", true),
            paint_value(
                &format!("failed to copy {}: {err}", commit.full_hash),
                AnsiColor::Yellow,
                true
            )
        ),
    }
}

fn resize_inline_log(
    app: &mut App,
    terminal: &mut TerminalGuard,
    requested_height: u16,
) -> Result<()> {
    if app.fullscreen {
        app.status = "fullscreen".to_string();
        return Ok(());
    }

    let max_height = size().map(|(_, rows)| rows).unwrap_or(app.height);
    let height = requested_height.max(MIN_HEIGHT).min(max_height);
    if height == app.height {
        return Ok(());
    }

    let (_, rows) = size().unwrap_or((0, app.height));
    let anchor_y = resize_anchor_row(
        app.last_drawn_top,
        app.last_drawn_height,
        height,
        rows.max(1),
    );
    terminal.resize(height, anchor_y)?;
    app.height = height;
    app.last_drawn_height = height;
    app.last_drawn_top = anchor_y;
    app.status = format!("height {height}");
    Ok(())
}

fn fullscreen_log(app: &mut App, terminal: &mut TerminalGuard) -> Result<()> {
    let (_, rows) = size().unwrap_or((0, app.height));
    let target = rows.max(MIN_HEIGHT);
    if app.fullscreen {
        app.status = "fullscreen".to_string();
        return Ok(());
    }

    if app.restore_height.is_none() {
        app.restore_height = Some(app.height);
    }
    terminal.enter_fullscreen()?;
    app.height = target;
    app.last_drawn_height = target;
    app.last_drawn_top = 0;
    app.fullscreen = true;
    app.status = "fullscreen".to_string();
    Ok(())
}

fn restore_inline_log(app: &mut App, terminal: &mut TerminalGuard) -> Result<()> {
    let Some(height) = app.restore_height.take() else {
        app.status = "already inline".to_string();
        return Ok(());
    };
    if app.fullscreen {
        terminal.leave_fullscreen(height)?;
        app.height = height.max(MIN_HEIGHT);
        app.last_drawn_height = app.height;
        app.last_drawn_top = 0;
        app.fullscreen = false;
    } else {
        resize_inline_log(app, terminal, height)?;
    }
    app.status = format!("height {}", app.height);
    Ok(())
}

fn resize_anchor_row(
    previous_top: u16,
    previous_height: u16,
    new_height: u16,
    terminal_rows: u16,
) -> u16 {
    let anchor = if new_height < previous_height {
        previous_top.saturating_add(previous_height - new_height)
    } else {
        previous_top
    };
    anchor.min(terminal_rows.saturating_sub(1))
}

fn pending_status(pending: &[KeyPress]) -> String {
    let keys = pending
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{keys} ...")
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.last_drawn_top = area.y;
    app.last_drawn_height = area.height;
    frame.render_widget(Clear, area);
    let block = Block::default().title(" inlog ").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [list_area, status_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let height = list_area.height.max(1) as usize;
    app.visible_height = height;
    app.ensure_selection_visible(height);
    frame.render_widget(Paragraph::new(render_rows(app, height)), list_area);
    frame.render_widget(Paragraph::new(app.status.clone()), status_area);

    let selected_row = app.row_offset_for_commit(app.selected);
    if selected_row >= app.scroll_y && selected_row < app.scroll_y + height {
        frame.set_cursor_position(Position::new(
            list_area.x,
            list_area.y + (selected_row - app.scroll_y) as u16,
        ));
    }
}

fn render_rows(app: &App, height: usize) -> Vec<Line<'static>> {
    let mut all_rows = Vec::new();
    for (idx, commit) in app.commits.iter().enumerate() {
        let selected = idx == app.selected;
        all_rows.push(render_commit_row(
            commit,
            selected,
            app.expanded == Some(idx),
        ));
        if app.expanded == Some(idx) {
            all_rows.extend(expanded_commit_lines(commit));
        }
    }
    let mut rows = all_rows
        .into_iter()
        .skip(app.scroll_y)
        .take(height)
        .collect::<Vec<_>>();
    while rows.len() < height {
        rows.push(Line::from(Span::styled(
            "~",
            Style::default().fg(Color::DarkGray),
        )));
    }
    rows
}

fn render_commit_row(commit: &Commit, selected: bool, expanded: bool) -> Line<'static> {
    let style = if selected {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    };
    let marker = if expanded { "/" } else { " " };
    let refs = if commit.refs.is_empty() {
        String::new()
    } else {
        format!(" ({})", commit.refs)
    };
    Line::from(vec![
        Span::styled(marker, style.fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled(format!("{:<9}", commit.short_hash), style.fg(Color::Cyan)),
        Span::styled(
            format!("{:<11}", commit.author_date),
            style.fg(Color::Green),
        ),
        Span::styled(format!("{:<18}", truncate(&commit.author_name, 17)), style),
        Span::styled(format!("{}{}", commit.subject, refs), style),
    ])
}

fn expanded_commit_lines(commit: &Commit) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::raw("    commit "),
            Span::styled(commit.full_hash.clone(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![Span::raw(format!(
            "    Author: {} <{}>",
            commit.author_name, commit.author_email
        ))]),
        Line::from(vec![Span::raw(format!(
            "    AuthorDate: {}",
            commit.author_date_full
        ))]),
        Line::from(vec![Span::raw(format!(
            "    CommitDate: {}",
            commit.commit_date_full
        ))]),
    ];
    if !commit.refs.is_empty() {
        lines.push(Line::from(vec![Span::raw(format!(
            "    Refs: {}",
            commit.refs
        ))]));
    }
    if commit.body.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "    (no commit message body)",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        lines.extend(
            commit
                .body
                .lines()
                .map(|line| Line::from(vec![Span::raw(format!("    {line}"))])),
        );
    }
    lines
}

fn expanded_commit_line_count(commit: &Commit) -> usize {
    let base = 4;
    let refs = usize::from(!commit.refs.is_empty());
    let message = if commit.body.is_empty() {
        1
    } else {
        commit.body.lines().count()
    };
    base + refs + message
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = text.chars().take(max.saturating_sub(1)).collect::<String>();
    out.push('~');
    out
}

fn truncate_with_dots(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = text.chars().take(max).collect::<String>();
    out.truncate(out.trim_end().len());
    out.push_str("...");
    out
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    mode: TerminalMode,
    clear_on_drop: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalMode {
    Inline,
    Fullscreen,
}

impl TerminalGuard {
    fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        let mut terminal = Self::new_inline_terminal(height.max(MIN_HEIGHT))?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            mode: TerminalMode::Inline,
            clear_on_drop: true,
        })
    }

    fn new_inline_terminal(height: u16) -> Result<Terminal<CrosstermBackend<Stdout>>> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        Ok(Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(1)),
            },
        )?)
    }

    fn new_fullscreen_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        Ok(Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?)
    }

    fn resize(&mut self, height: u16, anchor_y: u16) -> Result<()> {
        debug_assert_eq!(self.mode, TerminalMode::Inline);
        self.terminal.clear()?;
        io::stdout().execute(MoveTo(0, anchor_y))?;
        self.terminal = Self::new_inline_terminal(height)?;
        Ok(())
    }

    fn enter_fullscreen(&mut self) -> Result<()> {
        if self.mode == TerminalMode::Fullscreen {
            return Ok(());
        }
        self.terminal.clear()?;
        io::stdout().execute(EnterAlternateScreen)?;
        self.terminal = Self::new_fullscreen_terminal()?;
        self.terminal.clear()?;
        self.mode = TerminalMode::Fullscreen;
        Ok(())
    }

    fn leave_fullscreen(&mut self, height: u16) -> Result<()> {
        if self.mode == TerminalMode::Inline {
            return Ok(());
        }
        self.terminal.clear()?;
        io::stdout().execute(LeaveAlternateScreen)?;
        self.terminal = Self::new_inline_terminal(height)?;
        self.terminal.clear()?;
        self.mode = TerminalMode::Inline;
        Ok(())
    }

    fn collapse(&mut self, anchor_y: u16) -> Result<()> {
        self.terminal.clear()?;
        io::stdout().execute(MoveTo(0, anchor_y))?;
        self.terminal = Self::new_inline_terminal(1)?;
        self.terminal.clear()?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.clear_on_drop {
            let _ = self.terminal.clear();
        }
        if self.mode == TerminalMode::Fullscreen {
            let _ = io::stdout().execute(LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        let _ = io::stdout().execute(Show);
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> String {
        format!(
            "{RECORD_SEP}abcdef0123456789{FIELD_SEP}abcdef0{FIELD_SEP}Ryan Daum{FIELD_SEP}ryan@example.com{FIELD_SEP}2026-05-12{FIELD_SEP}2026-05-12T10:00:00-04:00{FIELD_SEP}2026-05-12T10:01:00-04:00{FIELD_SEP}HEAD -> main{FIELD_SEP}Subject here{FIELD_SEP}body one\nbody two\n"
        )
    }

    fn sample_commits(count: usize) -> Vec<Commit> {
        let base = Commit::parse_many(&sample_record()).unwrap().remove(0);
        (0..count)
            .map(|idx| Commit {
                full_hash: format!("{idx:040x}"),
                short_hash: format!("{idx:07x}"),
                subject: format!("Commit {idx}"),
                body: format!("body {idx}\nmore {idx}"),
                ..base.clone()
            })
            .collect()
    }

    #[test]
    fn parses_structured_log_records() {
        let commits = Commit::parse_many(&sample_record()).unwrap();

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].full_hash, "abcdef0123456789");
        assert_eq!(commits[0].refs, "HEAD -> main");
        assert_eq!(commits[0].body, "body one\nbody two");
    }

    #[test]
    fn formats_exit_summary_without_color() {
        let commit = Commit::parse_many(&sample_record()).unwrap().remove(0);
        let summary = commit.summary_text(false);

        assert!(summary.contains("commit abcdef0123456789    author Ryan Daum <ryan@example.com>"));
        assert!(summary.contains("author date 2026-05-12T10:00:00-04:00"));
        assert!(summary.contains("commit date 2026-05-12T10:01:00-04:00"));
        assert!(summary.contains("refs HEAD -> main"));
        assert!(summary.contains("\nsubject Subject here\n"));
        assert!(!summary.contains("body one"));
        assert!(!summary.contains("\x1b["));
    }

    #[test]
    fn exit_summary_truncates_long_subject() {
        let mut commit = Commit::parse_many(&sample_record()).unwrap().remove(0);
        commit.subject = "123456789 123456789 123456789 123456789 123456789 more".to_string();
        let summary = commit.summary_text(false);

        assert!(summary.contains("subject 123456789 123456789 123456789 123456789 123456789..."));
        assert!(!summary.contains(" more"));
    }

    #[test]
    fn rejects_graph_and_adds_default_limit() {
        assert!(normalize_git_args(vec!["--graph".to_string()]).is_err());
        assert_eq!(normalize_git_args(Vec::new()).unwrap(), vec!["-n", "500"]);
        assert_eq!(
            normalize_git_args(vec!["-n10".to_string()]).unwrap(),
            vec!["-n10"]
        );
        assert_eq!(
            normalize_git_args(vec!["--author=ryan".to_string(), "-25".to_string()]).unwrap(),
            vec!["--author=ryan", "-25"]
        );
        assert_eq!(
            normalize_git_args(vec!["--author=ryan".to_string()]).unwrap(),
            vec!["-n", "500", "--author=ryan"]
        );
    }

    #[test]
    fn search_finds_body_and_wraps() {
        let mut commits = Commit::parse_many(&sample_record()).unwrap();
        commits.push(Commit {
            subject: "Other".to_string(),
            body: "Needle".to_string(),
            ..commits[0].clone()
        });
        let mut app = App::new(commits, DEFAULT_HEIGHT);

        app.begin_search(SearchDirection::Forward);
        for ch in "needle".chars() {
            app.search_insert_char(ch);
        }

        assert_eq!(app.selected, 1);
    }

    #[test]
    fn selected_expand_toggles_only_selected_commit() {
        let commits = Commit::parse_many(&sample_record()).unwrap();
        let mut app = App::new(commits, DEFAULT_HEIGHT);

        app.toggle_expand();
        assert_eq!(app.expanded, Some(0));
        app.toggle_expand();
        assert_eq!(app.expanded, None);
    }

    #[test]
    fn accept_marks_selection_for_exit_copy() {
        let commits = Commit::parse_many(&sample_record()).unwrap();
        let mut app = App::new(commits, DEFAULT_HEIGHT);

        app.accept();
        assert!(app.accepted);
    }

    #[test]
    fn expanded_lines_are_visible_without_body() {
        let mut commit = Commit::parse_many(&sample_record()).unwrap().remove(0);
        commit.body.clear();
        let lines = expanded_commit_lines(&commit);

        assert!(
            lines
                .iter()
                .any(|line| format!("{line:?}").contains("no commit message body"))
        );
    }

    #[test]
    fn scrolls_past_expanded_commit_at_bottom() {
        let mut app = App::new(sample_commits(8), DEFAULT_HEIGHT);
        app.selected = 3;
        app.toggle_expand();
        app.ensure_selection_visible(4);

        app.select_delta(1);
        app.ensure_selection_visible(4);

        assert_eq!(app.selected, 4);
        assert_eq!(app.scroll_y, app.row_offset_for_commit(4));
    }

    #[test]
    fn bottom_scroll_does_not_start_inside_expanded_details() {
        let mut app = App::new(sample_commits(8), DEFAULT_HEIGHT);
        app.selected = 2;
        app.toggle_expand();
        app.ensure_selection_visible(6);

        app.select_delta(2);
        app.ensure_selection_visible(6);

        assert_eq!(app.selected, 4);
        assert_eq!(app.scroll_y, app.row_offset_for_commit(3));
    }
}
