use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ropey::Rope;

use crate::config::{Keymap, KeymapMatch};

use super::buffer::{line_selection_range, resize_anchor_row};
use super::editor::{Editor, SearchDirection};
use super::input::{Outcome, handle_inline_action};
use super::text_mode::TextMode;
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
        text_mode: TextMode::Plain {
            fill_column: DEFAULT_FILL_COLUMN,
        },
        last_drawn_height: DEFAULT_HEIGHT,
        last_drawn_top: 0,
        search: None,
        mark: None,
        undo_stack: Vec::new(),
        redo_stack: Vec::new(),
    }
}

fn commit_editor_with(text: &str) -> Editor {
    let mut editor = editor_with(text);
    editor.path = PathBuf::from(".git/COMMIT_EDITMSG");
    editor.text_mode = TextMode::CommitMessage {
        subject_column: 50,
        body_column: 72,
        comment_prefix: "#",
    };
    editor
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

    editor.text_mode = TextMode::Plain { fill_column: 24 };
    editor.fill_paragraph();

    assert_eq!(
        editor.buffer.to_string(),
        "before\n\none two three four five\nsix seven eight nine ten\neleven twelve continued\nhere\n\nafter"
    );
    assert_eq!(editor.status, "filled paragraph to 24");
}

#[test]
fn fill_paragraph_preserves_common_indent() {
    let mut editor = editor_with("    alpha beta gamma delta epsilon\n    zeta eta theta");

    editor.text_mode = TextMode::Plain { fill_column: 22 };
    editor.fill_paragraph();

    assert_eq!(
        editor.buffer.to_string(),
        "    alpha beta gamma\n    delta epsilon zeta\n    eta theta"
    );
}

#[test]
fn fill_paragraph_is_one_undo_step() {
    let mut editor = editor_with("one two three four five");

    editor.text_mode = TextMode::Plain { fill_column: 12 };
    editor.fill_paragraph();
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

#[test]
fn commit_mode_warns_when_subject_exceeds_fifty_columns() {
    let editor = commit_editor_with("this subject is intentionally longer than fifty columns\n");

    assert_eq!(editor.display_status(), "commit subject 55/50");
}

#[test]
fn commit_mode_warns_when_body_lacks_separator() {
    let editor = commit_editor_with("short subject\nbody text starts immediately\n");

    assert_eq!(
        editor.display_status(),
        "commit body needs blank line after subject"
    );
}

#[test]
fn commit_mode_warns_when_body_line_exceeds_limit() {
    let editor = commit_editor_with(
        "short subject\n\nthis body line is intentionally long enough to exceed the seventy two column limit here\n",
    );

    assert_eq!(editor.display_status(), "commit body line 3 87/72");
}

#[test]
fn commit_mode_fill_wraps_body_to_seventy_two_columns() {
    let mut editor = commit_editor_with(
        "short subject\n\nthis body line is intentionally long enough to exceed the seventy two column limit and should wrap cleanly\n",
    );
    editor.cursor_line = 2;

    editor.fill_paragraph();

    assert_eq!(
        editor.buffer.to_string(),
        "short subject\n\nthis body line is intentionally long enough to exceed the seventy two\ncolumn limit and should wrap cleanly\n"
    );
    assert_eq!(editor.status, "filled paragraph to 72");
}

#[test]
fn commit_mode_does_not_fill_subject_or_comments() {
    let mut editor = commit_editor_with(
        "short subject\n\n# comment line with enough words that it would wrap if comments were fillable\n",
    );

    editor.cursor_line = 0;
    editor.fill_paragraph();
    assert_eq!(editor.status, "no paragraph");

    editor.cursor_line = 2;
    editor.fill_paragraph();
    assert_eq!(editor.status, "no paragraph");
}

#[test]
fn commit_mode_auto_wraps_body_after_typed_space() {
    let mut editor = commit_editor_with("short subject\n\nalpha beta gamma delta");
    editor.text_mode = TextMode::CommitMessage {
        subject_column: 50,
        body_column: 20,
        comment_prefix: "#",
    };
    editor.cursor_line = 2;
    editor.cursor_col = editor.line_len();

    editor.insert_char(' ');

    assert_eq!(
        editor.buffer.to_string(),
        "short subject\n\nalpha beta gamma\ndelta "
    );
    assert_eq!((editor.cursor_line, editor.cursor_col), (3, 6));
}

#[test]
fn commit_mode_auto_wraps_indented_body_with_same_indent() {
    let mut editor = commit_editor_with("short subject\n\n  alpha beta gamma delta");
    editor.text_mode = TextMode::CommitMessage {
        subject_column: 50,
        body_column: 22,
        comment_prefix: "#",
    };
    editor.cursor_line = 2;
    editor.cursor_col = editor.line_len();

    editor.insert_char(' ');

    assert_eq!(
        editor.buffer.to_string(),
        "short subject\n\n  alpha beta gamma\n  delta "
    );
    assert_eq!((editor.cursor_line, editor.cursor_col), (3, 8));
}

#[test]
fn auto_wrap_does_not_apply_to_plain_text_or_commit_subject() {
    let mut editor = editor_with("alpha beta gamma delta");
    editor.text_mode = TextMode::Plain { fill_column: 20 };
    editor.cursor_col = editor.line_len();

    editor.insert_char(' ');

    assert_eq!(editor.buffer.to_string(), "alpha beta gamma delta ");

    let mut editor = commit_editor_with("alpha beta gamma delta");
    editor.text_mode = TextMode::CommitMessage {
        subject_column: 20,
        body_column: 20,
        comment_prefix: "#",
    };
    editor.cursor_col = editor.line_len();

    editor.insert_char(' ');

    assert_eq!(editor.buffer.to_string(), "alpha beta gamma delta ");
}
