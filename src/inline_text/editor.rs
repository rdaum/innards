use std::fs;
use std::io;
use std::ops::Range;
use std::path::PathBuf;

use anyhow::{Context, Result};
use ropey::Rope;

use crate::config::KeyPress;

use super::buffer::{
    buffer_line_count, char_len, common_indent_len, find_in_line_forward, find_in_line_reverse,
    line_len_chars, line_start_char, line_text, wrap_words,
};
use super::{MIN_HEIGHT, Mode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SearchDirection {
    Forward,
    Reverse,
}

pub(super) struct SearchState {
    query: String,
    direction: SearchDirection,
    origin_line: usize,
    origin_col: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BufferPoint {
    line: usize,
    col: usize,
}

#[derive(Clone)]
pub(super) struct EditorSnapshot {
    buffer: Rope,
    cursor_line: usize,
    cursor_col: usize,
    dirty: bool,
    mark: Option<BufferPoint>,
}

pub(super) struct Editor {
    pub(super) path: PathBuf,
    pub(super) buffer: Rope,
    pub(super) cursor_line: usize,
    pub(super) cursor_col: usize,
    pub(super) scroll_y: usize,
    pub(super) scroll_x: usize,
    pub(super) kill_ring: String,
    pub(super) dirty: bool,
    pub(super) status: String,
    pub(super) pending_keys: Vec<KeyPress>,
    pub(super) height: u16,
    pub(super) restore_height: Option<u16>,
    pub(super) fullscreen: bool,
    pub(super) fill_column: usize,
    pub(super) last_drawn_height: u16,
    pub(super) last_drawn_top: u16,
    pub(super) search: Option<SearchState>,
    pub(super) mark: Option<BufferPoint>,
    pub(super) undo_stack: Vec<EditorSnapshot>,
    pub(super) redo_stack: Vec<EditorSnapshot>,
}

impl Editor {
    pub(super) fn open(
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

    pub(super) fn save(&mut self) -> Result<()> {
        let mut file = fs::File::create(&self.path)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.buffer
            .write_to(&mut file)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        self.dirty = false;
        self.status = format!("saved {}", self.path.display());
        Ok(())
    }

    pub(super) fn line_len(&self) -> usize {
        line_len_chars(&self.buffer, self.cursor_line)
    }

    pub(super) fn line_count(&self) -> usize {
        buffer_line_count(&self.buffer)
    }

    pub(super) fn cursor_char_idx(&self) -> usize {
        line_start_char(&self.buffer, self.cursor_line) + self.cursor_col
    }

    pub(super) fn cursor_point(&self) -> BufferPoint {
        BufferPoint {
            line: self.cursor_line,
            col: self.cursor_col,
        }
    }

    pub(super) fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            buffer: self.buffer.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
            dirty: self.dirty,
            mark: self.mark,
        }
    }

    pub(super) fn restore_snapshot(&mut self, snapshot: EditorSnapshot) {
        self.buffer = snapshot.buffer;
        self.cursor_line = snapshot.cursor_line;
        self.cursor_col = snapshot.cursor_col;
        self.dirty = snapshot.dirty;
        self.mark = snapshot.mark;
        self.search = None;
        self.pending_keys.clear();
        self.clamp_cursor();
    }

    pub(super) fn record_edit(&mut self) {
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();
    }

    pub(super) fn undo(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            self.status = "no undo".to_string();
            return;
        };
        self.redo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status = "undo".to_string();
    }

    pub(super) fn redo(&mut self) {
        let Some(snapshot) = self.redo_stack.pop() else {
            self.status = "no redo".to_string();
            return;
        };
        self.undo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot);
        self.status = "redo".to_string();
    }

    pub(super) fn point_char_idx(&self, point: BufferPoint) -> usize {
        let line = point.line.min(self.line_count().saturating_sub(1));
        line_start_char(&self.buffer, line) + point.col.min(line_len_chars(&self.buffer, line))
    }

    pub(super) fn set_cursor_from_char_idx(&mut self, char_idx: usize) {
        let char_idx = char_idx.min(self.buffer.len_chars());
        self.cursor_line = self.buffer.char_to_line(char_idx);
        self.cursor_col = char_idx.saturating_sub(line_start_char(&self.buffer, self.cursor_line));
        self.clamp_cursor();
    }

    pub(super) fn active_region(&self) -> Option<Range<usize>> {
        let mark = self.mark?;
        let mark = self.point_char_idx(mark);
        let cursor = self.cursor_char_idx();
        if mark == cursor {
            return None;
        }
        Some(mark.min(cursor)..mark.max(cursor))
    }

    pub(super) fn delete_active_region(&mut self) -> bool {
        let Some(region) = self.active_region() else {
            return false;
        };
        self.remove_region(region);
        self.mark_dirty();
        true
    }

    pub(super) fn remove_region(&mut self, region: Range<usize>) -> String {
        let start = region.start;
        let text = self.buffer.slice(region.clone()).to_string();
        self.buffer.remove(region);
        self.set_cursor_from_char_idx(start);
        text
    }

    pub(super) fn clamp_cursor(&mut self) {
        self.cursor_line = self.cursor_line.min(self.line_count().saturating_sub(1));
        self.cursor_col = self.cursor_col.min(self.line_len());
    }

    pub(super) fn ensure_cursor_visible(&mut self, text_height: usize, text_width: usize) {
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

    pub(super) fn insert_char(&mut self, ch: char) {
        self.record_edit();
        self.delete_active_region();
        self.buffer.insert_char(self.cursor_char_idx(), ch);
        self.cursor_col += 1;
        self.mark_dirty();
    }

    pub(super) fn insert_newline(&mut self) {
        self.record_edit();
        self.delete_active_region();
        self.buffer.insert_char(self.cursor_char_idx(), '\n');
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.mark_dirty();
    }

    pub(super) fn backspace(&mut self) {
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

    pub(super) fn delete_char(&mut self) {
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

    pub(super) fn kill_to_eol(&mut self) {
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

    pub(super) fn toggle_mark(&mut self) {
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

    pub(super) fn cancel_mark(&mut self) {
        if self.mark.take().is_some() {
            self.status = "mark cancelled".to_string();
        } else {
            self.status = "quit".to_string();
        }
        self.pending_keys.clear();
    }

    pub(super) fn copy_region(&mut self) {
        let Some(region) = self.active_region() else {
            self.status = "no active region".to_string();
            return;
        };
        self.kill_ring = self.buffer.slice(region).to_string();
        self.status = "region copied".to_string();
        self.pending_keys.clear();
    }

    pub(super) fn kill_region(&mut self) {
        let Some(region) = self.active_region() else {
            self.status = "no active region".to_string();
            return;
        };
        self.record_edit();
        self.kill_ring = self.remove_region(region);
        self.mark_dirty();
        self.status = "region killed".to_string();
    }

    pub(super) fn yank(&mut self) {
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

    pub(super) fn fill_paragraph(&mut self, column: usize) {
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

    pub(super) fn paragraph_bounds(&self, line: usize) -> Option<(usize, usize)> {
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

    pub(super) fn line_is_blank(&self, line: usize) -> bool {
        line_text(&self.buffer, line).trim().is_empty()
    }

    pub(super) fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.line_len();
        }
    }

    pub(super) fn move_right(&mut self) {
        if self.cursor_col < self.line_len() {
            self.cursor_col += 1;
        } else if self.cursor_line + 1 < self.line_count() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    pub(super) fn move_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.clamp_cursor();
        }
    }

    pub(super) fn move_down(&mut self) {
        if self.cursor_line + 1 < self.line_count() {
            self.cursor_line += 1;
            self.clamp_cursor();
        }
    }

    pub(super) fn move_word_left(&mut self) {
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

    pub(super) fn move_word_right(&mut self) {
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

    pub(super) fn page_up(&mut self) {
        let amount = self.page_amount();
        self.cursor_line = self.cursor_line.saturating_sub(amount);
        self.clamp_cursor();
    }

    pub(super) fn page_down(&mut self) {
        let amount = self.page_amount();
        self.cursor_line = (self.cursor_line + amount).min(self.line_count().saturating_sub(1));
        self.clamp_cursor();
    }

    pub(super) fn page_amount(&self) -> usize {
        usize::from(self.height.saturating_sub(3)).max(1)
    }

    pub(super) fn begin_search(&mut self, direction: SearchDirection) {
        self.pending_keys.clear();
        self.search = Some(SearchState {
            query: String::new(),
            direction,
            origin_line: self.cursor_line,
            origin_col: self.cursor_col,
        });
        self.update_search_status();
    }

    pub(super) fn cancel_search(&mut self) {
        if let Some(search) = self.search.take() {
            self.cursor_line = search.origin_line;
            self.cursor_col = search.origin_col;
            self.status = "search cancelled".to_string();
        }
    }

    pub(super) fn finish_search(&mut self) {
        if self.search.take().is_some() {
            self.status = "search done".to_string();
        }
    }

    pub(super) fn search_insert_char(&mut self, ch: char) {
        if let Some(search) = self.search.as_mut() {
            search.query.push(ch);
        }
        self.apply_incremental_search();
    }

    pub(super) fn search_backspace(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.query.pop();
        }
        self.apply_incremental_search();
    }

    pub(super) fn search_repeat(&mut self, direction: SearchDirection) {
        if let Some(search) = self.search.as_mut() {
            search.direction = direction;
        }
        self.apply_repeated_search();
    }

    pub(super) fn apply_incremental_search(&mut self) {
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

    pub(super) fn apply_repeated_search(&mut self) {
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

    pub(super) fn find_from(
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

    pub(super) fn update_search_status(&mut self) {
        if let Some(search) = self.search.as_ref() {
            let label = match search.direction {
                SearchDirection::Forward => "I-search",
                SearchDirection::Reverse => "I-search backward",
            };
            self.status = format!("{label}: {}", search.query);
        }
    }

    pub(super) fn find_forward(
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

    pub(super) fn find_reverse(
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

    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
        self.pending_keys.clear();
        self.search = None;
        self.mark = None;
        self.status = "modified".to_string();
    }
}
