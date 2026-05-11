use std::env;
use std::fs;
use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::ExecutableCommand;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use innards::config::{InnardsConfig, KeyPress, Keymap, KeymapMatch};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

const DEFAULT_HEIGHT: u16 = 18;
const MIN_HEIGHT: u16 = 6;

const REBASE_KEY_BINDINGS: &[(&str, &[&str])] = &[
    ("save", &["ctrl-x ctrl-s"]),
    ("quit", &["ctrl-x ctrl-c"]),
    ("select_prev", &["ctrl-p", "up"]),
    ("select_next", &["ctrl-n", "down"]),
    ("move_up", &["alt-p"]),
    ("move_down", &["alt-n"]),
    ("page_up", &["pageup", "alt-v"]),
    ("page_down", &["pagedown", "ctrl-v"]),
    ("pick", &["p"]),
    ("reword", &["r"]),
    ("edit", &["e"]),
    ("squash", &["s"]),
    ("fixup", &["f"]),
    ("drop", &["d"]),
    ("exec", &["x"]),
    ("prompt_save", &["s"]),
    ("prompt_abort", &["a"]),
    ("prompt_cancel", &["c", "esc"]),
];

const REBASE_ACTIONS: &[&str] = &[
    "save",
    "quit",
    "select_prev",
    "select_next",
    "move_up",
    "move_down",
    "page_up",
    "page_down",
    "pick",
    "reword",
    "edit",
    "squash",
    "fixup",
    "drop",
    "exec",
];

const PROMPT_ACTIONS: &[&str] = &["prompt_save", "prompt_abort", "prompt_cancel"];

fn main() -> Result<()> {
    let config = Config::parse(env::args().skip(1))?;
    let innards_config = InnardsConfig::load()?;
    let mut keymap = Keymap::from_defaults(REBASE_KEY_BINDINGS)?;
    keymap.apply_overrides(&innards_config.keybindings.rebase)?;

    let source = fs::read_to_string(&config.path)
        .with_context(|| format!("failed to read {}", config.path.display()))?;
    let mut app = App::new(config.path.clone(), source);
    let mut terminal = TerminalGuard::enter(config.height)?;
    let outcome = run_event_loop(&mut terminal.terminal, &mut app, &keymap)?;
    drop(terminal);

    match outcome {
        Outcome::Save => {
            fs::write(&config.path, app.serialize())
                .with_context(|| format!("failed to write {}", config.path.display()))?;
            Ok(())
        }
        Outcome::Abort => std::process::exit(1),
    }
}

struct Config {
    path: PathBuf,
    height: u16,
}

impl Config {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut height = DEFAULT_HEIGHT;
        let mut path = None;
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
            } else if path.is_none() {
                path = Some(PathBuf::from(arg));
            } else {
                return Err(anyhow!("unexpected argument: {arg}"));
            }
        }

        let path = path.ok_or_else(|| anyhow!("usage: inbase [--height N] TODO_FILE"))?;
        Ok(Self { path, height })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct App {
    path: PathBuf,
    todo: Todo,
    selected: usize,
    scroll_y: usize,
    status: String,
    pending_keys: Vec<KeyPress>,
    quit_prompt: bool,
}

impl App {
    fn new(path: PathBuf, source: String) -> Self {
        let todo = Todo::parse(&source);
        let selected = todo.first_editable().unwrap_or(0);
        Self {
            path,
            todo,
            selected,
            scroll_y: 0,
            status: "Ctrl-X Ctrl-S save  Ctrl-X Ctrl-C quit".to_string(),
            pending_keys: Vec::new(),
            quit_prompt: false,
        }
    }

    fn serialize(&self) -> String {
        self.todo.serialize()
    }

    fn select_delta(&mut self, delta: isize) {
        let Some(next) = self.todo.editable_delta(self.selected, delta) else {
            self.status = "no editable todo line".to_string();
            return;
        };
        self.selected = next;
        self.status.clear();
    }

    fn move_selected(&mut self, delta: isize) {
        let Some(next) = self.todo.move_editable(self.selected, delta) else {
            self.status = "cannot move further".to_string();
            return;
        };
        self.selected = next;
        self.status = "moved".to_string();
    }

    fn set_selected_action(&mut self, action: Action) {
        let Some(line) = self.todo.lines.get_mut(self.selected) else {
            return;
        };
        match line.set_action(action) {
            Ok(()) => self.status = format!("set {}", action.name()),
            Err(err) => self.status = err,
        }
    }

    fn ensure_selection_visible(&mut self, height: usize) {
        if self.selected < self.scroll_y {
            self.scroll_y = self.selected;
        } else if self.selected >= self.scroll_y.saturating_add(height) {
            self.scroll_y = self.selected.saturating_sub(height.saturating_sub(1));
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Todo {
    lines: Vec<TodoLine>,
    trailing_newline: bool,
}

impl Todo {
    fn parse(source: &str) -> Self {
        Self {
            lines: source.lines().map(TodoLine::parse).collect(),
            trailing_newline: source.ends_with('\n'),
        }
    }

    fn serialize(&self) -> String {
        let mut out = self
            .lines
            .iter()
            .map(TodoLine::serialize)
            .collect::<Vec<_>>()
            .join("\n");
        if self.trailing_newline {
            out.push('\n');
        }
        out
    }

    fn first_editable(&self) -> Option<usize> {
        self.lines.iter().position(TodoLine::is_editable)
    }

    fn editable_delta(&self, current: usize, delta: isize) -> Option<usize> {
        if delta < 0 {
            self.lines
                .iter()
                .take(current)
                .enumerate()
                .rev()
                .filter(|(_, line)| line.is_editable())
                .nth(delta.unsigned_abs() - 1)
                .map(|(idx, _)| idx)
        } else if delta > 0 {
            self.lines
                .iter()
                .enumerate()
                .skip(current + 1)
                .filter(|(_, line)| line.is_editable())
                .nth(delta as usize - 1)
                .map(|(idx, _)| idx)
        } else if self.lines.get(current).is_some_and(TodoLine::is_editable) {
            Some(current)
        } else {
            self.first_editable()
        }
    }

    fn move_editable(&mut self, current: usize, delta: isize) -> Option<usize> {
        let target = self.editable_delta(current, delta)?;
        self.lines.swap(current, target);
        Some(target)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TodoLine {
    raw: String,
    kind: TodoLineKind,
}

impl TodoLine {
    fn parse(line: &str) -> Self {
        let kind = parse_editable(line)
            .map(TodoLineKind::Editable)
            .unwrap_or(TodoLineKind::Raw);
        Self {
            raw: line.to_string(),
            kind,
        }
    }

    fn is_editable(&self) -> bool {
        matches!(self.kind, TodoLineKind::Editable(_))
    }

    fn serialize(&self) -> String {
        match &self.kind {
            TodoLineKind::Editable(line) if line.action_changed => {
                if line.rest.is_empty() {
                    line.action.name().to_string()
                } else {
                    format!("{} {}", line.action.name(), line.rest)
                }
            }
            _ => self.raw.clone(),
        }
    }

    fn set_action(&mut self, action: Action) -> std::result::Result<(), String> {
        let TodoLineKind::Editable(line) = &mut self.kind else {
            return Err("not an editable todo line".to_string());
        };
        if action == Action::Exec && line.action != Action::Exec {
            return Err("exec only applies to existing exec lines".to_string());
        }
        line.action = action;
        line.action_changed = true;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TodoLineKind {
    Editable(EditableLine),
    Raw,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditableLine {
    action: Action,
    rest: String,
    action_changed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
    Exec,
    Break,
    Label,
    Reset,
    Merge,
}

impl Action {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "p" | "pick" => Self::Pick,
            "r" | "reword" => Self::Reword,
            "e" | "edit" => Self::Edit,
            "s" | "squash" => Self::Squash,
            "f" | "fixup" => Self::Fixup,
            "d" | "drop" => Self::Drop,
            "x" | "exec" => Self::Exec,
            "b" | "break" => Self::Break,
            "l" | "label" => Self::Label,
            "t" | "reset" => Self::Reset,
            "m" | "merge" => Self::Merge,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Edit => "edit",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
            Self::Exec => "exec",
            Self::Break => "break",
            Self::Label => "label",
            Self::Reset => "reset",
            Self::Merge => "merge",
        }
    }
}

fn parse_editable(line: &str) -> Option<EditableLine> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (action, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((action, rest)) => (action, rest.trim_start().to_string()),
        None => (trimmed, String::new()),
    };
    Some(EditableLine {
        action: Action::parse(action)?,
        rest,
        action_changed: false,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Save,
    Abort,
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    keymap: &Keymap,
) -> Result<Outcome> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let Some(outcome) = handle_key(app, key, keymap) {
                        return Ok(outcome);
                    }
                }
                _ => {}
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent, keymap: &Keymap) -> Option<Outcome> {
    let actions = if app.quit_prompt {
        PROMPT_ACTIONS
    } else {
        REBASE_ACTIONS
    };

    match keymap.match_key_for_actions(actions, &app.pending_keys, &key) {
        KeymapMatch::Prefix => {
            if let Some(key) = keymap.keypress_from_event(&key) {
                app.pending_keys.push(key);
                app.status = pending_status(&app.pending_keys);
            }
            None
        }
        KeymapMatch::Action(action) => {
            app.pending_keys.clear();
            handle_action(app, action.as_str())
        }
        KeymapMatch::None if !app.pending_keys.is_empty() => {
            app.pending_keys.clear();
            app.status = "unknown key sequence".to_string();
            None
        }
        KeymapMatch::None => None,
    }
}

fn handle_action(app: &mut App, action: &str) -> Option<Outcome> {
    if app.quit_prompt {
        match action {
            "prompt_save" => return Some(Outcome::Save),
            "prompt_abort" => return Some(Outcome::Abort),
            "prompt_cancel" => {
                app.quit_prompt = false;
                app.status = "quit cancelled".to_string();
            }
            _ => {}
        }
        return None;
    }

    match action {
        "save" => return Some(Outcome::Save),
        "quit" => {
            app.quit_prompt = true;
            app.status = "save, abort, or cancel?".to_string();
        }
        "select_prev" => app.select_delta(-1),
        "select_next" => app.select_delta(1),
        "move_up" => app.move_selected(-1),
        "move_down" => app.move_selected(1),
        "page_up" => app.select_delta(-10),
        "page_down" => app.select_delta(10),
        "pick" => app.set_selected_action(Action::Pick),
        "reword" => app.set_selected_action(Action::Reword),
        "edit" => app.set_selected_action(Action::Edit),
        "squash" => app.set_selected_action(Action::Squash),
        "fixup" => app.set_selected_action(Action::Fixup),
        "drop" => app.set_selected_action(Action::Drop),
        "exec" => app.set_selected_action(Action::Exec),
        _ => {}
    }
    None
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
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(format!(" inbase: {} ", app.path.display()))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [list_area, prompt_area, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(if app.quit_prompt { 1 } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(inner);
    let height = list_area.height.max(1) as usize;
    app.ensure_selection_visible(height);

    let rows = render_rows(app, height);
    frame.render_widget(Paragraph::new(rows), list_area);

    if app.quit_prompt {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "s",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" save  "),
                Span::styled(
                    "a",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" abort  "),
                Span::styled(
                    "c",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" cancel"),
            ])),
            prompt_area,
        );
    }

    frame.render_widget(Paragraph::new(app.status.clone()), status_area);
    if app.selected >= app.scroll_y && app.selected < app.scroll_y + height {
        frame.set_cursor_position(Position::new(
            list_area.x,
            list_area.y + (app.selected - app.scroll_y) as u16,
        ));
    }
}

fn render_rows(app: &App, height: usize) -> Vec<Line<'static>> {
    let end = (app.scroll_y + height).min(app.todo.lines.len());
    let mut rows = Vec::with_capacity(height);
    for idx in app.scroll_y..end {
        rows.push(render_line(idx, &app.todo.lines[idx], idx == app.selected));
    }
    while rows.len() < height {
        rows.push(Line::from(Span::styled(
            "~",
            Style::default().fg(Color::DarkGray),
        )));
    }
    rows
}

fn render_line(idx: usize, line: &TodoLine, selected: bool) -> Line<'static> {
    let selection_style = if selected {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    };
    let prefix_color = if selected {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let raw_color = if selected {
        Color::White
    } else {
        Color::DarkGray
    };
    let prefix = Span::styled(format!("{:>3} ", idx + 1), selection_style.fg(prefix_color));
    match &line.kind {
        TodoLineKind::Editable(editable) => Line::from(vec![
            prefix,
            Span::styled(
                format!("{:<7}", editable.action.name()),
                selection_style.fg(action_color(editable.action)),
            ),
            Span::styled(format!(" {}", editable.rest), selection_style),
        ]),
        TodoLineKind::Raw => Line::from(vec![
            prefix,
            Span::styled(line.raw.clone(), selection_style.fg(raw_color)),
        ]),
    }
}

fn action_color(action: Action) -> Color {
    match action {
        Action::Pick => Color::Green,
        Action::Reword => Color::Yellow,
        Action::Edit => Color::Cyan,
        Action::Squash | Action::Fixup => Color::Magenta,
        Action::Drop => Color::Red,
        Action::Exec | Action::Break | Action::Label | Action::Reset | Action::Merge => Color::Blue,
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(MIN_HEIGHT)),
            },
        )?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.clear();
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        let _ = io::stdout().execute(Show);
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(source: &str) -> App {
        App::new(PathBuf::from("git-rebase-todo"), source.to_string())
    }

    #[test]
    fn parses_and_round_trips_todo_lines() {
        let source = "pick abc first\n\n# comment\nexec cargo test\nunknown thing\n";
        let todo = Todo::parse(source);

        assert_eq!(todo.serialize(), source);
        assert_eq!(todo.first_editable(), Some(0));
        assert!(matches!(
            todo.lines[3].kind,
            TodoLineKind::Editable(EditableLine {
                action: Action::Exec,
                ..
            })
        ));
    }

    #[test]
    fn action_changes_serialize_canonical_command() {
        let mut app = app_with("pick abc first\n");

        app.set_selected_action(Action::Reword);

        assert_eq!(app.serialize(), "reword abc first\n");
        assert_eq!(app.status, "set reword");
    }

    #[test]
    fn exec_key_only_applies_to_existing_exec_lines() {
        let mut app = app_with("pick abc first\nexec cargo test\n");

        app.set_selected_action(Action::Exec);
        assert_eq!(app.status, "exec only applies to existing exec lines");

        app.select_delta(1);
        app.set_selected_action(Action::Exec);
        assert_eq!(app.status, "set exec");
    }

    #[test]
    fn moves_editable_lines_across_comments_without_moving_comments() {
        let mut app = app_with("pick aaa first\n# keep me\npick bbb second\n");
        app.selected = 2;

        app.move_selected(-1);

        assert_eq!(app.selected, 0);
        assert_eq!(
            app.serialize(),
            "pick bbb second\n# keep me\npick aaa first\n"
        );
    }

    #[test]
    fn move_boundaries_report_status() {
        let mut app = app_with("pick aaa first\n");

        app.move_selected(-1);

        assert_eq!(app.status, "cannot move further");
        assert_eq!(app.serialize(), "pick aaa first\n");
    }

    #[test]
    fn selection_skips_raw_lines() {
        let mut app = app_with("# comment\npick aaa first\n\npick bbb second\n");

        assert_eq!(app.selected, 1);
        app.select_delta(1);
        assert_eq!(app.selected, 3);
        app.select_delta(-1);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn quit_prompt_transitions() {
        let mut app = app_with("pick aaa first\n");

        assert_eq!(handle_action(&mut app, "quit"), None);
        assert!(app.quit_prompt);
        assert_eq!(handle_action(&mut app, "prompt_cancel"), None);
        assert!(!app.quit_prompt);
        assert_eq!(handle_action(&mut app, "quit"), None);
        assert_eq!(
            handle_action(&mut app, "prompt_abort"),
            Some(Outcome::Abort)
        );
    }

    #[test]
    fn save_action_returns_save_outcome() {
        let mut app = app_with("pick aaa first\n");

        assert_eq!(handle_action(&mut app, "save"), Some(Outcome::Save));
    }
}
