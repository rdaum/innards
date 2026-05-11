use std::env;
use std::fs;
use std::io::{self, Stdout, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{InnardsConfig, KeyPress, Keymap, KeymapMatch};
use anyhow::{Context, Result, anyhow};
use crossterm::ExecutableCommand;
use crossterm::cursor::{MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use ropey::Rope;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

mod render;

const DEFAULT_HEIGHT: u16 = 16;
const MIN_HEIGHT: u16 = 5;
const DEFAULT_FILL_COLUMN: usize = 80;

const INLINE_KEY_BINDINGS: &[(&str, &[&str])] = &[
    ("quit", &["ctrl-x ctrl-c"]),
    ("save", &["ctrl-x ctrl-s"]),
    ("search_forward", &["ctrl-s"]),
    ("search_reverse", &["ctrl-r"]),
    ("cancel_search", &["esc", "ctrl-g"]),
    ("finish_search", &["enter"]),
    ("cancel_mark", &["ctrl-g"]),
    ("set_mark", &["ctrl-space", "null"]),
    ("undo", &["ctrl-/", "ctrl-_", "ctrl-7"]),
    ("redo", &["ctrl-?"]),
    ("line_start", &["ctrl-a", "home"]),
    ("line_end", &["ctrl-e", "end"]),
    ("word_left", &["alt-b", "ctrl-left"]),
    ("word_right", &["alt-f", "ctrl-right"]),
    ("char_left", &["ctrl-b", "left"]),
    ("char_right", &["ctrl-f", "right"]),
    ("line_up", &["ctrl-p", "up"]),
    ("line_down", &["ctrl-n", "down"]),
    ("page_up", &["alt-v", "pageup"]),
    ("page_down", &["ctrl-v", "pagedown"]),
    ("copy_region", &["alt-w"]),
    ("kill_region", &["ctrl-w"]),
    ("kill_to_eol", &["ctrl-k"]),
    ("yank", &["ctrl-y"]),
    ("delete_char", &["ctrl-d", "delete"]),
    ("backspace", &["backspace"]),
    ("insert_newline", &["enter"]),
    ("insert_tab", &["tab"]),
    ("shrink_height", &["alt-up"]),
    ("grow_height", &["alt-down"]),
    ("fullscreen", &["ctrl-x 1"]),
    ("restore_inline", &["ctrl-x 0"]),
    ("fill_paragraph", &["alt-q"]),
    ("quit_view", &["esc", "q"]),
];

const INLINE_NORMAL_ACTIONS: &[&str] = &[
    "quit",
    "save",
    "search_forward",
    "search_reverse",
    "cancel_mark",
    "set_mark",
    "undo",
    "redo",
    "line_start",
    "line_end",
    "word_left",
    "word_right",
    "char_left",
    "char_right",
    "line_up",
    "line_down",
    "page_up",
    "page_down",
    "copy_region",
    "kill_region",
    "kill_to_eol",
    "yank",
    "delete_char",
    "backspace",
    "insert_newline",
    "insert_tab",
    "shrink_height",
    "grow_height",
    "fullscreen",
    "restore_inline",
    "fill_paragraph",
];
const INLINE_VIEW_ACTIONS: &[&str] = &[
    "quit",
    "quit_view",
    "search_forward",
    "search_reverse",
    "line_start",
    "line_end",
    "word_left",
    "word_right",
    "char_left",
    "char_right",
    "line_up",
    "line_down",
    "page_up",
    "page_down",
    "shrink_height",
    "grow_height",
    "fullscreen",
    "restore_inline",
];
const INLINE_SEARCH_ACTIONS: &[&str] = &[
    "quit",
    "save",
    "fullscreen",
    "restore_inline",
    "search_forward",
    "search_reverse",
    "cancel_search",
    "finish_search",
    "backspace",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Edit,
    View,
}

impl Mode {
    fn title(self) -> &'static str {
        match self {
            Self::Edit => "inmacs",
            Self::View => "inpage",
        }
    }

    fn initial_status(self) -> &'static str {
        match self {
            Self::Edit => "Ctrl-S search  Ctrl-R reverse-search  Ctrl-X Ctrl-S save",
            Self::View => "Ctrl-S search  Ctrl-R reverse-search  Ctrl-X Ctrl-C quit",
        }
    }

    fn is_editable(self) -> bool {
        matches!(self, Self::Edit)
    }
}

pub fn run(mode: Mode) -> Result<()> {
    let config = Config::parse(env::args().skip(1), mode)?;
    let innards_config = InnardsConfig::load()?;
    let fill_column = innards_config
        .inmacs
        .fill_column
        .unwrap_or(DEFAULT_FILL_COLUMN);
    let mut keymap = Keymap::from_defaults(INLINE_KEY_BINDINGS)?;
    keymap.apply_overrides(&innards_config.keybindings.inline)?;

    let mut app = Editor::open(
        config.path.clone(),
        config.line,
        config.height,
        fill_column,
        mode,
    )?;
    let syntax = SyntaxHighlighter::new(&config.path)?;

    let mut terminal = TerminalGuard::enter(config.height)?;
    terminal
        .terminal
        .draw(|frame| render::draw(frame, &mut app, &syntax, mode))?;
    let outcome = run_editor(&mut terminal, &mut app, &syntax, mode, &keymap)?;
    drop(terminal);

    match outcome {
        Outcome::Quit => Ok(()),
    }
}

struct Config {
    path: PathBuf,
    height: u16,
    line: Option<usize>,
}

impl Config {
    fn parse(args: impl Iterator<Item = String>, mode: Mode) -> Result<Self> {
        let mut height = DEFAULT_HEIGHT;
        let mut line = None;
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
            } else if arg == "--line" {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--line requires a line number"))?;
                line = Some(parse_line_number(&value)?);
            } else if let Some(value) = arg.strip_prefix("--line=") {
                line = Some(parse_line_number(value)?);
            } else if let Some(value) = arg.strip_prefix('+') {
                if !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()) {
                    line = Some(parse_line_number(value)?);
                } else {
                    path = Some(PathBuf::from(arg));
                }
            } else if path.is_none() {
                path = Some(PathBuf::from(arg));
            } else {
                return Err(anyhow!("unexpected argument: {arg}"));
            }
        }

        let path =
            path.ok_or_else(|| anyhow!("usage: {} [--height N] [+LINE] FILE", mode.title()))?;
        Ok(Self { path, height, line })
    }
}

fn parse_line_number(value: &str) -> Result<usize> {
    let line = value
        .parse::<usize>()
        .with_context(|| format!("invalid line number: {value}"))?;
    Ok(line.max(1))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchDirection {
    Forward,
    Reverse,
}

struct SearchState {
    query: String,
    direction: SearchDirection,
    origin_line: usize,
    origin_col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BufferPoint {
    line: usize,
    col: usize,
}

#[derive(Clone)]
struct EditorSnapshot {
    buffer: Rope,
    cursor_line: usize,
    cursor_col: usize,
    dirty: bool,
    mark: Option<BufferPoint>,
}

struct Editor {
    path: PathBuf,
    buffer: Rope,
    cursor_line: usize,
    cursor_col: usize,
    scroll_y: usize,
    scroll_x: usize,
    kill_ring: String,
    dirty: bool,
    status: String,
    pending_keys: Vec<KeyPress>,
    height: u16,
    restore_height: Option<u16>,
    fullscreen: bool,
    fill_column: usize,
    last_drawn_height: u16,
    last_drawn_top: u16,
    search: Option<SearchState>,
    mark: Option<BufferPoint>,
    undo_stack: Vec<EditorSnapshot>,
    redo_stack: Vec<EditorSnapshot>,
}

impl Editor {
    fn open(
        path: PathBuf,
        line: Option<usize>,
        height: u16,
        fill_column: usize,
        mode: Mode,
    ) -> Result<Self> {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let buffer = Rope::from_str(&content);
        let cursor_line = line
            .map(|line| {
                line.saturating_sub(1)
                    .min(buffer_line_count(&buffer).saturating_sub(1))
            })
            .unwrap_or(0);

        Ok(Self {
            path,
            buffer,
            cursor_line,
            cursor_col: 0,
            scroll_y: 0,
            scroll_x: 0,
            kill_ring: String::new(),
            dirty: false,
            status: mode.initial_status().to_string(),
            pending_keys: Vec::new(),
            height: height.max(MIN_HEIGHT),
            restore_height: None,
            fullscreen: false,
            fill_column,
            last_drawn_height: height.max(MIN_HEIGHT),
            last_drawn_top: 0,
            search: None,
            mark: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        })
    }

    fn save(&mut self) -> Result<()> {
        let mut file = fs::File::create(&self.path)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.buffer
            .write_to(&mut file)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.dirty = false;
        self.status = format!("saved {}", self.path.display());
        Ok(())
    }

    fn line_len(&self) -> usize {
        line_len_chars(&self.buffer, self.cursor_line)
    }

    fn line_count(&self) -> usize {
        buffer_line_count(&self.buffer)
    }

    fn cursor_char_idx(&self) -> usize {
        line_start_char(&self.buffer, self.cursor_line) + self.cursor_col
    }

    fn cursor_point(&self) -> BufferPoint {
        BufferPoint {
            line: self.cursor_line,
            col: self.cursor_col,
        }
    }

    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            buffer: self.buffer.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
            dirty: self.dirty,
            mark: self.mark,
        }
    }

    fn restore_snapshot(&mut self, snapshot: EditorSnapshot) {
        self.buffer = snapshot.buffer;
        self.cursor_line = snapshot.cursor_line;
        self.cursor_col = snapshot.cursor_col;
        self.dirty = snapshot.dirty;
        self.mark = snapshot.mark;
        self.search = None;
        self.pending_keys.clear();
        self.clamp_cursor();
    }

    fn record_edit(&mut self) {
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            self.status = "no undo".to_string();
            return;
        };
        self.redo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status = "undo".to_string();
    }

    fn redo(&mut self) {
        let Some(snapshot) = self.redo_stack.pop() else {
            self.status = "no redo".to_string();
            return;
        };
        self.undo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status = "redo".to_string();
    }

    fn point_char_idx(&self, point: BufferPoint) -> usize {
        let line = point.line.min(self.line_count().saturating_sub(1));
        line_start_char(&self.buffer, line) + point.col.min(line_len_chars(&self.buffer, line))
    }

    fn set_cursor_from_char_idx(&mut self, char_idx: usize) {
        let char_idx = char_idx.min(self.buffer.len_chars());
        self.cursor_line = self.buffer.char_to_line(char_idx);
        self.cursor_col = char_idx.saturating_sub(line_start_char(&self.buffer, self.cursor_line));
        self.clamp_cursor();
    }

    fn active_region(&self) -> Option<Range<usize>> {
        let mark = self.mark?;
        let mark = self.point_char_idx(mark);
        let cursor = self.cursor_char_idx();
        if mark == cursor {
            return None;
        }
        Some(mark.min(cursor)..mark.max(cursor))
    }

    fn delete_active_region(&mut self) -> bool {
        let Some(region) = self.active_region() else {
            return false;
        };
        self.remove_region(region);
        self.mark_dirty();
        true
    }

    fn remove_region(&mut self, region: Range<usize>) -> String {
        let start = region.start;
        let text = self.buffer.slice(region.clone()).to_string();
        self.buffer.remove(region);
        self.set_cursor_from_char_idx(start);
        text
    }

    fn clamp_cursor(&mut self) {
        self.cursor_line = self.cursor_line.min(self.line_count().saturating_sub(1));
        self.cursor_col = self.cursor_col.min(self.line_len());
    }

    fn ensure_cursor_visible(&mut self, text_height: usize, text_width: usize) {
        if self.cursor_line < self.scroll_y {
            self.scroll_y = self.cursor_line;
        } else if self.cursor_line >= self.scroll_y.saturating_add(text_height) {
            self.scroll_y = self
                .cursor_line
                .saturating_sub(text_height.saturating_sub(1));
        }

        if self.cursor_col < self.scroll_x {
            self.scroll_x = self.cursor_col;
        } else if self.cursor_col >= self.scroll_x.saturating_add(text_width) {
            self.scroll_x = self.cursor_col.saturating_sub(text_width.saturating_sub(1));
        }
    }

    fn insert_char(&mut self, ch: char) {
        self.record_edit();
        self.delete_active_region();
        self.buffer.insert_char(self.cursor_char_idx(), ch);
        self.cursor_col += 1;
        self.mark_dirty();
    }

    fn insert_newline(&mut self) {
        self.record_edit();
        self.delete_active_region();
        self.buffer.insert_char(self.cursor_char_idx(), '\n');
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.mark_dirty();
    }

    fn backspace(&mut self) {
        if self.active_region().is_some() {
            self.record_edit();
            self.delete_active_region();
            return;
        }
        if self.cursor_col > 0 {
            self.record_edit();
            let end = self.cursor_char_idx();
            self.buffer.remove(end - 1..end);
            self.cursor_col -= 1;
            self.mark_dirty();
        } else if self.cursor_line > 0 {
            self.record_edit();
            let current_start = line_start_char(&self.buffer, self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.line_len();
            let previous_content_end =
                line_start_char(&self.buffer, self.cursor_line) + self.cursor_col;
            self.buffer.remove(previous_content_end..current_start);
            self.mark_dirty();
        }
    }

    fn delete_char(&mut self) {
        if self.active_region().is_some() {
            self.record_edit();
            self.delete_active_region();
            return;
        }
        if self.cursor_col < self.line_len() {
            self.record_edit();
            let start = self.cursor_char_idx();
            self.buffer.remove(start..start + 1);
            self.mark_dirty();
        } else if self.cursor_line + 1 < self.line_count() {
            self.record_edit();
            let start = self.cursor_char_idx();
            let end = line_start_char(&self.buffer, self.cursor_line + 1);
            self.buffer.remove(start..end);
            self.mark_dirty();
        }
    }

    fn kill_to_eol(&mut self) {
        if self.cursor_col < self.line_len() {
            self.record_edit();
            let start = self.cursor_char_idx();
            let end = line_start_char(&self.buffer, self.cursor_line) + self.line_len();
            self.kill_ring = self.buffer.slice(start..end).to_string();
            self.buffer.remove(start..end);
        } else if self.cursor_line + 1 < self.line_count() {
            self.record_edit();
            let start = self.cursor_char_idx();
            let end = line_start_char(&self.buffer, self.cursor_line + 1);
            self.kill_ring = self.buffer.slice(start..end).to_string();
            self.buffer.remove(start..end);
        } else {
            self.kill_ring.clear();
            return;
        }
        self.mark_dirty();
    }

    fn toggle_mark(&mut self) {
        let point = self.cursor_point();
        if self.mark == Some(point) {
            self.mark = None;
            self.status = "mark cleared".to_string();
        } else {
            self.mark = Some(point);
            self.status = "mark set".to_string();
        }
        self.pending_keys.clear();
    }

    fn cancel_mark(&mut self) {
        if self.mark.take().is_some() {
            self.status = "mark cancelled".to_string();
        } else {
            self.status = "quit".to_string();
        }
        self.pending_keys.clear();
    }

    fn copy_region(&mut self) {
        let Some(region) = self.active_region() else {
            self.status = "no active region".to_string();
            return;
        };
        self.kill_ring = self.buffer.slice(region).to_string();
        self.status = "region copied".to_string();
        self.pending_keys.clear();
    }

    fn kill_region(&mut self) {
        let Some(region) = self.active_region() else {
            self.status = "no active region".to_string();
            return;
        };
        self.record_edit();
        self.kill_ring = self.remove_region(region);
        self.mark_dirty();
        self.status = "region killed".to_string();
    }

    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        self.record_edit();
        self.delete_active_region();
        let text = self.kill_ring.clone();
        for ch in text.chars() {
            if ch == '\n' {
                self.buffer.insert_char(self.cursor_char_idx(), '\n');
                self.cursor_line += 1;
                self.cursor_col = 0;
            } else {
                self.buffer.insert_char(self.cursor_char_idx(), ch);
                self.cursor_col += 1;
            }
        }
        self.mark_dirty();
    }

    fn fill_paragraph(&mut self, column: usize) {
        let Some((start_line, end_line)) = self.paragraph_bounds(self.cursor_line) else {
            self.status = "no paragraph".to_string();
            return;
        };

        let original_cursor = self.cursor_char_idx();
        let start = line_start_char(&self.buffer, start_line);
        let last_line = end_line - 1;
        let end =
            line_start_char(&self.buffer, last_line) + line_len_chars(&self.buffer, last_line);
        let original = self.buffer.slice(start..end).to_string();
        let lines: Vec<String> = (start_line..end_line)
            .map(|line| line_text(&self.buffer, line))
            .collect();

        let indent_len = common_indent_len(&lines);
        let indent: String = lines
            .iter()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.chars().take(indent_len).collect())
            .unwrap_or_default();
        let mut words = Vec::new();
        for line in &lines {
            let content: String = line.chars().skip(indent_len).collect();
            words.extend(content.split_whitespace().map(str::to_string));
        }

        if words.is_empty() {
            self.status = "no paragraph".to_string();
            return;
        }

        let wrapped = wrap_words(&words, &indent, column);
        if wrapped == original {
            self.status = format!("already filled to {column}");
            return;
        }

        self.record_edit();
        self.buffer.remove(start..end);
        self.buffer.insert(start, &wrapped);
        let cursor = start
            + original_cursor
                .saturating_sub(start)
                .min(char_len(&wrapped));
        self.set_cursor_from_char_idx(cursor);
        self.mark_dirty();
        self.status = format!("filled paragraph to {column}");
    }

    fn paragraph_bounds(&self, line: usize) -> Option<(usize, usize)> {
        if self.line_is_blank(line) {
            return None;
        }

        let mut start = line;
        while start > 0 && !self.line_is_blank(start - 1) {
            start -= 1;
        }

        let mut end = line + 1;
        while end < self.line_count() && !self.line_is_blank(end) {
            end += 1;
        }

        Some((start, end))
    }

    fn line_is_blank(&self, line: usize) -> bool {
        line_text(&self.buffer, line).trim().is_empty()
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.line_len();
        }
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.line_len() {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.line_count() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.clamp_cursor();
        }
    }

    fn move_down(&mut self) {
        if self.cursor_line + 1 < self.line_count() {
            self.cursor_line += 1;
            self.clamp_cursor();
        }
    }

    fn move_word_left(&mut self) {
        if self.cursor_col == 0 {
            self.move_left();
            return;
        }
        let chars: Vec<char> = line_text(&self.buffer, self.cursor_line).chars().collect();
        let mut col = self.cursor_col;
        while col > 0 && chars[col - 1].is_whitespace() {
            col -= 1;
        }
        while col > 0 && !chars[col - 1].is_whitespace() {
            col -= 1;
        }
        self.cursor_col = col;
    }

    fn move_word_right(&mut self) {
        let chars: Vec<char> = line_text(&self.buffer, self.cursor_line).chars().collect();
        let mut col = self.cursor_col;
        while col < chars.len() && !chars[col].is_whitespace() {
            col += 1;
        }
        while col < chars.len() && chars[col].is_whitespace() {
            col += 1;
        }
        self.cursor_col = col;
    }

    fn page_up(&mut self) {
        let amount = self.page_amount();
        self.cursor_line = self.cursor_line.saturating_sub(amount);
        self.clamp_cursor();
    }

    fn page_down(&mut self) {
        let amount = self.page_amount();
        self.cursor_line = (self.cursor_line + amount).min(self.line_count().saturating_sub(1));
        self.clamp_cursor();
    }

    fn page_amount(&self) -> usize {
        usize::from(self.height.saturating_sub(3)).max(1)
    }

    fn begin_search(&mut self, direction: SearchDirection) {
        self.pending_keys.clear();
        self.search = Some(SearchState {
            query: String::new(),
            direction,
            origin_line: self.cursor_line,
            origin_col: self.cursor_col,
        });
        self.update_search_status();
    }

    fn cancel_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.cursor_line = search.origin_line;
            self.cursor_col = search.origin_col;
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
        self.apply_incremental_search();
    }

    fn search_backspace(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.query.pop();
        }
        self.apply_incremental_search();
    }

    fn search_repeat(&mut self, direction: SearchDirection) {
        if let Some(search) = self.search.as_mut() {
            search.direction = direction;
        }
        self.apply_repeated_search();
    }

    fn apply_incremental_search(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        if search.query.is_empty() {
            self.cursor_line = search.origin_line;
            self.cursor_col = search.origin_col;
            self.update_search_status();
            return;
        }

        let found = self.find_from(
            search.direction,
            &search.query,
            search.origin_line,
            search.origin_col,
        );

        if let Some((line, col)) = found {
            self.cursor_line = line;
            self.cursor_col = col;
        }
        self.update_search_status();
    }

    fn apply_repeated_search(&mut self) {
        let Some(search) = self.search.as_ref() else {
            return;
        };
        if search.query.is_empty() {
            self.update_search_status();
            return;
        }

        let (line, col) = match search.direction {
            SearchDirection::Forward => (self.cursor_line, self.cursor_col.saturating_add(1)),
            SearchDirection::Reverse => (self.cursor_line, self.cursor_col),
        };
        let found = self.find_from(search.direction, &search.query, line, col);

        if let Some((line, col)) = found {
            self.cursor_line = line;
            self.cursor_col = col;
        }
        self.update_search_status();
    }

    fn find_from(
        &self,
        direction: SearchDirection,
        query: &str,
        start_line: usize,
        start_col: usize,
    ) -> Option<(usize, usize)> {
        match direction {
            SearchDirection::Forward => self.find_forward(query, start_line, start_col),
            SearchDirection::Reverse => self.find_reverse(query, start_line, start_col),
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

    fn find_forward(
        &self,
        query: &str,
        start_line: usize,
        start_col: usize,
    ) -> Option<(usize, usize)> {
        let line_count = self.line_count();
        for line_idx in start_line..line_count {
            let start = if line_idx == start_line { start_col } else { 0 };
            if let Some(col) =
                find_in_line_forward(&line_text(&self.buffer, line_idx), query, start)
            {
                return Some((line_idx, col));
            }
        }
        for line_idx in 0..start_line {
            if let Some(col) = find_in_line_forward(&line_text(&self.buffer, line_idx), query, 0) {
                return Some((line_idx, col));
            }
        }
        None
    }

    fn find_reverse(
        &self,
        query: &str,
        start_line: usize,
        start_col: usize,
    ) -> Option<(usize, usize)> {
        for line_idx in (0..=start_line).rev() {
            let end = if line_idx == start_line {
                start_col
            } else {
                line_len_chars(&self.buffer, line_idx)
            };
            if let Some(col) = find_in_line_reverse(&line_text(&self.buffer, line_idx), query, end)
            {
                return Some((line_idx, col));
            }
        }
        for line_idx in ((start_line + 1)..self.line_count()).rev() {
            let line = line_text(&self.buffer, line_idx);
            if let Some(col) = find_in_line_reverse(&line, query, char_len(&line)) {
                return Some((line_idx, col));
            }
        }
        None
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.pending_keys.clear();
        self.search = None;
        self.mark = None;
        self.status = "modified".to_string();
    }
}

struct SyntaxHighlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
    syntax_name: String,
}

impl SyntaxHighlighter {
    fn new(path: &PathBuf) -> Result<Self> {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .or_else(|| theme_set.themes.values().next())
            .cloned()
            .ok_or_else(|| anyhow!("no syntect themes available"))?;
        let syntax = syntax_set
            .find_syntax_for_file(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
        let syntax_name = syntax.name.clone();

        Ok(Self {
            syntax_set,
            theme,
            syntax_name,
        })
    }

    fn syntax(&self) -> &SyntaxReference {
        self.syntax_set
            .find_syntax_by_name(&self.syntax_name)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }
}

fn run_editor(
    terminal: &mut TerminalGuard,
    app: &mut Editor,
    syntax: &SyntaxHighlighter,
    mode: Mode,
    keymap: &Keymap,
) -> Result<Outcome> {
    loop {
        if event::poll(Duration::from_millis(80))? {
            match event::read() {
                Ok(Event::Key(key))
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let Some(outcome) = handle_key(app, key, terminal, mode, keymap)? {
                        return Ok(outcome);
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    app.status = format!("input error: {err}");
                }
            }
        }
        terminal
            .terminal
            .draw(|frame| render::draw(frame, app, syntax, mode))?;
    }
}

fn handle_key(
    app: &mut Editor,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
    mode: Mode,
    keymap: &Keymap,
) -> Result<Option<Outcome>> {
    if app.search.is_some() {
        if let Some(outcome) = handle_key_binding(app, key, terminal, mode, keymap)? {
            return Ok(outcome);
        }
        return handle_search_text_key(app, key);
    }

    if let Some(outcome) = handle_key_binding(app, key, terminal, mode, keymap)? {
        return Ok(outcome);
    }

    match key.code {
        KeyCode::Char(ch)
            if mode.is_editable()
                && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
        {
            app.insert_char(ch);
        }
        _ => {}
    }

    Ok(None)
}

fn handle_key_binding(
    app: &mut Editor,
    key: KeyEvent,
    terminal: &mut TerminalGuard,
    mode: Mode,
    keymap: &Keymap,
) -> Result<Option<Option<Outcome>>> {
    let actions = if app.search.is_some() {
        INLINE_SEARCH_ACTIONS
    } else if mode == Mode::View {
        INLINE_VIEW_ACTIONS
    } else {
        INLINE_NORMAL_ACTIONS
    };

    match keymap.match_key_for_actions(actions, &app.pending_keys, &key) {
        KeymapMatch::Prefix => {
            if let Some(key) = keymap.keypress_from_event(&key) {
                app.pending_keys.push(key);
                app.status = pending_status(&app.pending_keys);
            }
            Ok(Some(None))
        }
        KeymapMatch::Action(action) => {
            app.pending_keys.clear();
            handle_inline_action(app, Some(terminal), mode, action.as_str()).map(Some)
        }
        KeymapMatch::None if !app.pending_keys.is_empty() => {
            app.pending_keys.clear();
            app.status = "unknown key sequence".to_string();
            Ok(Some(None))
        }
        KeymapMatch::None => Ok(None),
    }
}

fn handle_inline_action(
    app: &mut Editor,
    terminal: Option<&mut TerminalGuard>,
    mode: Mode,
    action: &str,
) -> Result<Option<Outcome>> {
    match action {
        "quit" => Ok(Some(Outcome::Quit)),
        "quit_view" if mode == Mode::View => Ok(Some(Outcome::Quit)),
        "save" if mode.is_editable() => {
            app.save()?;
            Ok(None)
        }
        "save" => {
            app.status = "read-only".to_string();
            Ok(None)
        }
        "search_forward" if app.search.is_some() => {
            app.search_repeat(SearchDirection::Forward);
            Ok(None)
        }
        "search_forward" => {
            app.begin_search(SearchDirection::Forward);
            Ok(None)
        }
        "search_reverse" if app.search.is_some() => {
            app.search_repeat(SearchDirection::Reverse);
            Ok(None)
        }
        "search_reverse" => {
            app.begin_search(SearchDirection::Reverse);
            Ok(None)
        }
        "cancel_search" => {
            app.cancel_search();
            Ok(None)
        }
        "finish_search" => {
            app.finish_search();
            Ok(None)
        }
        "cancel_mark" => {
            app.cancel_mark();
            Ok(None)
        }
        "set_mark" => {
            app.toggle_mark();
            Ok(None)
        }
        "undo" if mode.is_editable() => {
            app.undo();
            Ok(None)
        }
        "redo" if mode.is_editable() => {
            app.redo();
            Ok(None)
        }
        "line_start" => {
            app.cursor_col = 0;
            Ok(None)
        }
        "line_end" => {
            app.cursor_col = app.line_len();
            Ok(None)
        }
        "word_left" => {
            app.move_word_left();
            Ok(None)
        }
        "word_right" => {
            app.move_word_right();
            Ok(None)
        }
        "char_left" => {
            app.move_left();
            Ok(None)
        }
        "char_right" => {
            app.move_right();
            Ok(None)
        }
        "line_up" => {
            app.move_up();
            Ok(None)
        }
        "line_down" => {
            app.move_down();
            Ok(None)
        }
        "page_up" => {
            app.page_up();
            Ok(None)
        }
        "page_down" => {
            app.page_down();
            Ok(None)
        }
        "copy_region" => {
            app.copy_region();
            Ok(None)
        }
        "kill_region" if mode.is_editable() => {
            app.kill_region();
            Ok(None)
        }
        "kill_to_eol" if mode.is_editable() => {
            app.kill_to_eol();
            Ok(None)
        }
        "yank" if mode.is_editable() => {
            app.yank();
            Ok(None)
        }
        "delete_char" if mode.is_editable() => {
            app.delete_char();
            Ok(None)
        }
        "backspace" if app.search.is_some() => {
            app.search_backspace();
            Ok(None)
        }
        "backspace" if mode.is_editable() => {
            app.backspace();
            Ok(None)
        }
        "insert_newline" if mode.is_editable() => {
            app.insert_newline();
            Ok(None)
        }
        "insert_tab" if mode.is_editable() => {
            for _ in 0..4 {
                app.insert_char(' ');
            }
            Ok(None)
        }
        "shrink_height" => {
            if let Some(terminal) = terminal {
                resize_inline_editor(app, terminal, app.height.saturating_sub(1))?;
            }
            Ok(None)
        }
        "grow_height" => {
            if let Some(terminal) = terminal {
                resize_inline_editor(app, terminal, app.height.saturating_add(1))?;
            }
            Ok(None)
        }
        "fullscreen" => {
            if let Some(terminal) = terminal {
                fullscreen_inline_editor(app, terminal)?;
            }
            Ok(None)
        }
        "restore_inline" => {
            if let Some(terminal) = terminal {
                restore_inline_editor(app, terminal)?;
            }
            Ok(None)
        }
        "fill_paragraph" if mode.is_editable() => {
            app.fill_paragraph(app.fill_column);
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn pending_status(pending: &[KeyPress]) -> String {
    let keys = pending
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{keys} ...")
}

fn handle_search_text_key(app: &mut Editor, key: KeyEvent) -> Result<Option<Outcome>> {
    match key.code {
        KeyCode::Char(ch) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            app.search_insert_char(ch);
        }
        _ => {}
    }

    Ok(None)
}

fn resize_inline_editor(
    app: &mut Editor,
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

fn fullscreen_inline_editor(app: &mut Editor, terminal: &mut TerminalGuard) -> Result<()> {
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

fn restore_inline_editor(app: &mut Editor, terminal: &mut TerminalGuard) -> Result<()> {
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
        resize_inline_editor(app, terminal, height)?;
    }
    app.status = format!("height {}", app.height);
    Ok(())
}

fn line_selection_range(
    app: &Editor,
    line: usize,
    active_region: Option<&Range<usize>>,
) -> Option<Range<usize>> {
    let active_region = active_region?;
    let line_start = line_start_char(&app.buffer, line);
    let line_end = line_start + line_len_chars(&app.buffer, line);
    let start = active_region.start.max(line_start);
    let end = active_region.end.min(line_end);
    if start < end {
        Some(start - line_start..end - line_start)
    } else {
        None
    }
}

fn buffer_line_count(buffer: &Rope) -> usize {
    buffer.len_lines().max(1)
}

fn line_start_char(buffer: &Rope, line: usize) -> usize {
    buffer.line_to_char(line.min(buffer_line_count(buffer).saturating_sub(1)))
}

fn line_len_chars(buffer: &Rope, line: usize) -> usize {
    let line = buffer.line(line.min(buffer_line_count(buffer).saturating_sub(1)));
    let mut len = line.len_chars();
    if len > 0 && line.char(len - 1) == '\n' {
        len -= 1;
        if len > 0 && line.char(len - 1) == '\r' {
            len -= 1;
        }
    }
    len
}

fn line_text(buffer: &Rope, line: usize) -> String {
    let line = line.min(buffer_line_count(buffer).saturating_sub(1));
    let len = line_len_chars(buffer, line);
    buffer.line(line).slice(..len).to_string()
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

fn common_indent_len(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|ch| ch.is_whitespace()).count())
        .min()
        .unwrap_or(0)
}

fn wrap_words(words: &[String], indent: &str, column: usize) -> String {
    let indent_len = char_len(indent);
    let target = column.max(indent_len + 1);
    let mut lines = Vec::new();
    let mut current = indent.to_string();
    let mut current_len = indent_len;

    for word in words {
        let word_len = char_len(word);
        let needs_space = current_len > indent_len;
        let next_len = current_len + usize::from(needs_space) + word_len;

        if needs_space && next_len > target {
            lines.push(current);
            current = indent.to_string();
            current.push_str(word);
            current_len = indent_len + word_len;
        } else {
            if needs_space {
                current.push(' ');
                current_len += 1;
            }
            current.push_str(word);
            current_len += word_len;
        }
    }

    lines.push(current);
    lines.join("\n")
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn byte_index(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn find_in_line_forward(line: &str, query: &str, start_col: usize) -> Option<usize> {
    let start = byte_index(line, start_col);
    line.get(start..)?
        .find(query)
        .map(|idx| start_col + char_len(&line[start..start + idx]))
}

fn find_in_line_reverse(line: &str, query: &str, end_col: usize) -> Option<usize> {
    let end = byte_index(line, end_col);
    line.get(..end)?
        .rfind(query)
        .map(|idx| char_len(&line[..idx]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keymap() -> Keymap {
        Keymap::from_defaults(INLINE_KEY_BINDINGS).unwrap()
    }

    fn editor_with(text: &str) -> Editor {
        Editor {
            path: PathBuf::from("test.txt"),
            buffer: Rope::from_str(text),
            cursor_line: 0,
            cursor_col: 0,
            scroll_y: 0,
            scroll_x: 0,
            kill_ring: String::new(),
            dirty: false,
            status: String::new(),
            pending_keys: Vec::new(),
            height: DEFAULT_HEIGHT,
            restore_height: None,
            fullscreen: false,
            fill_column: DEFAULT_FILL_COLUMN,
            last_drawn_height: DEFAULT_HEIGHT,
            last_drawn_top: 0,
            search: None,
            mark: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    #[test]
    fn backspace_at_line_start_joins_lines() {
        let mut editor = editor_with("abc\ndef");
        editor.cursor_line = 1;

        editor.backspace();

        assert_eq!(editor.buffer.to_string(), "abcdef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));
    }

    #[test]
    fn delete_at_eol_removes_whole_crlf_separator() {
        let mut editor = editor_with("abc\r\ndef");
        editor.cursor_col = 3;

        editor.delete_char();

        assert_eq!(editor.buffer.to_string(), "abcdef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));
    }

    #[test]
    fn kill_at_eol_keeps_separator_in_kill_ring() {
        let mut editor = editor_with("abc\ndef");
        editor.cursor_col = 3;

        editor.kill_to_eol();

        assert_eq!(editor.buffer.to_string(), "abcdef");
        assert_eq!(editor.kill_ring, "\n");
    }

    #[test]
    fn ctrl_w_kills_active_region() {
        let mut editor = editor_with("abc def");
        editor.cursor_col = 1;
        editor.toggle_mark();
        editor.cursor_col = 5;

        editor.kill_region();

        assert_eq!(editor.buffer.to_string(), "aef");
        assert_eq!(editor.kill_ring, "bc d");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 1));
        assert_eq!(editor.mark, None);
    }

    #[test]
    fn alt_w_copies_active_region_without_deleting() {
        let mut editor = editor_with("abc def");
        editor.cursor_col = 1;
        editor.toggle_mark();
        editor.cursor_col = 5;

        editor.copy_region();

        assert_eq!(editor.buffer.to_string(), "abc def");
        assert_eq!(editor.kill_ring, "bc d");
        assert!(editor.mark.is_some());
    }

    #[test]
    fn yank_replaces_active_region() {
        let mut editor = editor_with("abc def");
        editor.kill_ring = "XYZ".to_string();
        editor.cursor_col = 1;
        editor.toggle_mark();
        editor.cursor_col = 5;

        editor.yank();

        assert_eq!(editor.buffer.to_string(), "aXYZef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));
        assert_eq!(editor.mark, None);
    }

    #[test]
    fn undo_and_redo_restore_text_and_cursor() {
        let mut editor = editor_with("abc");
        editor.cursor_col = 3;

        editor.insert_char('d');
        assert_eq!(editor.buffer.to_string(), "abcd");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));

        editor.undo();
        assert_eq!(editor.buffer.to_string(), "abc");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 3));

        editor.redo();
        assert_eq!(editor.buffer.to_string(), "abcd");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));
    }

    #[test]
    fn yank_undo_is_single_step() {
        let mut editor = editor_with("abef");
        editor.kill_ring = "cd".to_string();
        editor.cursor_col = 2;

        editor.yank();
        assert_eq!(editor.buffer.to_string(), "abcdef");

        editor.undo();
        assert_eq!(editor.buffer.to_string(), "abef");
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 2));
    }

    #[test]
    fn fill_paragraph_wraps_current_paragraph() {
        let mut editor = editor_with(
            "before\n\none two three four five six seven eight nine ten eleven twelve\ncontinued here\n\nafter",
        );
        editor.cursor_line = 2;

        editor.fill_paragraph(24);

        assert_eq!(
            editor.buffer.to_string(),
            "before\n\none two three four five\nsix seven eight nine ten\neleven twelve continued\nhere\n\nafter"
        );
        assert_eq!(editor.status, "filled paragraph to 24");
    }

    #[test]
    fn fill_paragraph_preserves_common_indent() {
        let mut editor = editor_with("    alpha beta gamma delta epsilon\n    zeta eta theta");

        editor.fill_paragraph(22);

        assert_eq!(
            editor.buffer.to_string(),
            "    alpha beta gamma\n    delta epsilon zeta\n    eta theta"
        );
    }

    #[test]
    fn fill_paragraph_is_one_undo_step() {
        let mut editor = editor_with("one two three four five");

        editor.fill_paragraph(12);
        assert_eq!(editor.buffer.to_string(), "one two\nthree four\nfive");

        editor.undo();
        assert_eq!(editor.buffer.to_string(), "one two three four five");
    }

    #[test]
    fn down_arrow_after_mark_does_not_exit() {
        let mut editor = editor_with("abc\ndef");
        editor.toggle_mark();
        editor.move_down();

        assert_eq!((editor.cursor_line, editor.cursor_col), (1, 0));
        assert!(editor.active_region().is_some());
    }

    #[test]
    fn ctrl_g_cancels_active_mark() {
        let mut editor = editor_with("abc\ndef");
        editor.toggle_mark();
        editor.move_down();

        editor.cancel_mark();

        assert_eq!(editor.mark, None);
        assert!(editor.active_region().is_none());
        assert_eq!(editor.status, "mark cancelled");
    }

    #[test]
    fn view_mode_esc_and_q_quit() {
        let keymap = test_keymap();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(
            keymap.match_key_for_actions(INLINE_VIEW_ACTIONS, &[], &esc),
            KeymapMatch::Action("quit_view".to_string())
        );

        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        assert_eq!(
            keymap.match_key_for_actions(INLINE_VIEW_ACTIONS, &[], &q),
            KeymapMatch::Action("quit_view".to_string())
        );
    }

    #[test]
    fn edit_mode_esc_and_q_do_not_quit() {
        let keymap = test_keymap();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(
            keymap.match_key_for_actions(INLINE_NORMAL_ACTIONS, &[], &esc),
            KeymapMatch::None
        );

        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        assert_eq!(
            keymap.match_key_for_actions(INLINE_NORMAL_ACTIONS, &[], &q),
            KeymapMatch::None
        );
    }

    #[test]
    fn selection_range_after_region_does_not_underflow() {
        let mut editor = editor_with("abc\ndef\nghi");
        editor.toggle_mark();
        editor.move_down();

        assert_eq!(
            line_selection_range(&editor, 2, editor.active_region().as_ref()),
            None
        );
    }

    #[test]
    fn repeated_forward_search_jumps_to_next_match() {
        let mut editor = editor_with("foo bar foo");

        editor.begin_search(SearchDirection::Forward);
        for ch in "foo".chars() {
            editor.search_insert_char(ch);
        }
        editor.search_repeat(SearchDirection::Forward);

        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 8));
    }

    #[test]
    fn repeated_reverse_search_jumps_to_previous_match() {
        let mut editor = editor_with("foo bar foo");
        editor.cursor_col = 11;

        editor.begin_search(SearchDirection::Reverse);
        for ch in "foo".chars() {
            editor.search_insert_char(ch);
        }
        editor.search_repeat(SearchDirection::Reverse);

        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 0));
    }

    #[test]
    fn ctrl_x_exit_chord_works_while_searching() {
        let mut editor = editor_with("abc");
        editor.begin_search(SearchDirection::Forward);
        let keymap = test_keymap();

        let start = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            keymap.match_key_for_actions(INLINE_SEARCH_ACTIONS, &[], &start),
            KeymapMatch::Prefix
        );
        editor
            .pending_keys
            .push(keymap.keypress_from_event(&start).unwrap());

        let exit = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            keymap.match_key_for_actions(INLINE_SEARCH_ACTIONS, &editor.pending_keys, &exit),
            KeymapMatch::Action("quit".to_string())
        );
        assert_eq!(
            handle_inline_action(&mut editor, None, Mode::Edit, "quit").unwrap(),
            Some(Outcome::Quit)
        );
        assert!(editor.search.is_some());
    }

    #[test]
    fn ctrl_x_fullscreen_bindings_are_available() {
        let keymap = test_keymap();
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let one = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::empty());
        let zero = KeyEvent::new(KeyCode::Char('0'), KeyModifiers::empty());
        let pending = vec![keymap.keypress_from_event(&ctrl_x).unwrap()];

        assert_eq!(
            keymap.match_key_for_actions(INLINE_NORMAL_ACTIONS, &[], &ctrl_x),
            KeymapMatch::Prefix
        );
        assert_eq!(
            keymap.match_key_for_actions(INLINE_NORMAL_ACTIONS, &pending, &one),
            KeymapMatch::Action("fullscreen".to_string())
        );
        assert_eq!(
            keymap.match_key_for_actions(INLINE_NORMAL_ACTIONS, &pending, &zero),
            KeymapMatch::Action("restore_inline".to_string())
        );
    }

    #[test]
    fn ctrl_g_cancels_incremental_search() {
        let mut editor = editor_with("abc foo");
        editor.cursor_col = 2;
        editor.begin_search(SearchDirection::Forward);
        for ch in "foo".chars() {
            editor.search_insert_char(ch);
        }
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 4));

        assert_eq!(
            handle_inline_action(&mut editor, None, Mode::Edit, "cancel_search").unwrap(),
            None
        );

        assert!(editor.search.is_none());
        assert_eq!((editor.cursor_line, editor.cursor_col), (0, 2));
    }

    #[test]
    fn resize_anchor_preserves_top_when_growing() {
        assert_eq!(resize_anchor_row(8, 16, 17, 24), 8);
    }

    #[test]
    fn resize_anchor_preserves_bottom_when_shrinking() {
        assert_eq!(resize_anchor_row(8, 16, 12, 24), 12);
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    mode: TerminalMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalMode {
    Inline,
    Fullscreen,
}

impl TerminalGuard {
    fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        let terminal = Self::new_inline_terminal(height)?;
        Ok(Self {
            terminal,
            mode: TerminalMode::Inline,
        })
    }

    fn new_inline_terminal(height: u16) -> Result<Terminal<CrosstermBackend<Stdout>>> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(MIN_HEIGHT)),
            },
        )?;
        Ok(terminal)
    }

    fn new_fullscreen_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;
        Ok(terminal)
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
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.clear();
        if self.mode == TerminalMode::Fullscreen {
            let _ = io::stdout().execute(LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        let _ = io::stdout().execute(Show);
        let _ = io::stdout().flush();
    }
}
