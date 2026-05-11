use std::ops::Range;

use ropey::Rope;

use super::editor::Editor;

pub(super) fn line_selection_range(
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

pub(super) fn buffer_line_count(buffer: &Rope) -> usize {
    buffer.len_lines().max(1)
}

pub(super) fn line_start_char(buffer: &Rope, line: usize) -> usize {
    buffer.line_to_char(line.min(buffer_line_count(buffer).saturating_sub(1)))
}

pub(super) fn line_len_chars(buffer: &Rope, line: usize) -> usize {
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

pub(super) fn line_text(buffer: &Rope, line: usize) -> String {
    let line = line.min(buffer_line_count(buffer).saturating_sub(1));
    let len = line_len_chars(buffer, line);
    buffer.line(line).slice(..len).to_string()
}

pub(super) fn resize_anchor_row(
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

pub(super) fn common_indent_len(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.chars().take_while(|ch| ch.is_whitespace()).count())
        .min()
        .unwrap_or(0)
}

pub(super) fn wrap_words(words: &[String], indent: &str, column: usize) -> String {
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

pub(super) fn char_len(text: &str) -> usize {
    text.chars().count()
}

pub(super) fn byte_index(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

pub(super) fn find_in_line_forward(line: &str, query: &str, start_col: usize) -> Option<usize> {
    let start = byte_index(line, start_col);
    line.get(start..)?
        .find(query)
        .map(|idx| start_col + char_len(&line[start..start + idx]))
}

pub(super) fn find_in_line_reverse(line: &str, query: &str, end_col: usize) -> Option<usize> {
    let end = byte_index(line, end_col);
    line.get(..end)?
        .rfind(query)
        .map(|idx| char_len(&line[..idx]))
}
