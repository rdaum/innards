use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

use crate::config::{InnardsConfig, Keymap};

mod buffer;
mod editor;
mod input;
mod render;
mod syntax;
mod terminal;

#[cfg(test)]
mod tests;

use editor::Editor;
use input::{Outcome, run_editor};
use syntax::SyntaxHighlighter;
use terminal::TerminalGuard;

const DEFAULT_HEIGHT: u16 = 16;
pub(super) const MIN_HEIGHT: u16 = 5;
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
