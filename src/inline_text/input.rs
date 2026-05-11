use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::size;

use crate::config::{KeyPress, Keymap, KeymapMatch};

use super::buffer::resize_anchor_row;
use super::editor::{Editor, SearchDirection};
use super::render;
use super::syntax::SyntaxHighlighter;
use super::terminal::TerminalGuard;
use super::{INLINE_NORMAL_ACTIONS, INLINE_SEARCH_ACTIONS, INLINE_VIEW_ACTIONS, MIN_HEIGHT, Mode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Outcome {
    Quit,
}

pub(super) fn run_editor(
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

pub(super) fn handle_inline_action(
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
