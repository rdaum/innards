use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use syntect::easy::HighlightLines;

use super::Mode;
use super::buffer::{line_selection_range, line_text};
use super::editor::Editor;
use super::syntax::SyntaxHighlighter;

pub(super) fn draw(
    frame: &mut Frame<'_>,
    app: &mut Editor,
    syntax: &SyntaxHighlighter,
    mode: Mode,
) {
    let full_area = frame.area();
    frame.render_widget(Clear, full_area);
    let area = full_area;
    app.last_drawn_height = area.height;
    app.last_drawn_top = area.y;
    let block = Block::default()
        .title(format!(" {}: {} ", mode.title(), app.path.display()))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [text_area, status_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let digits = app.line_count().max(1).to_string().len();
    let prefix_width = digits + 2;
    let text_width = text_area.width.saturating_sub(prefix_width as u16).max(1) as usize;
    let text_height = text_area.height.max(1) as usize;
    app.ensure_cursor_visible(text_height, text_width);

    let lines = render_lines(app, syntax, text_height, text_width, prefix_width);
    frame.render_widget(Paragraph::new(lines), text_area);

    let dirty = if app.dirty { " *" } else { "" };
    let status = Line::from(vec![
        Span::styled(
            format!("{}:{}{}  ", app.cursor_line + 1, app.cursor_col + 1, dirty),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(app.display_status()),
    ]);
    frame.render_widget(Paragraph::new(status), status_area);

    if app.cursor_line >= app.scroll_y && app.cursor_line < app.scroll_y + text_height {
        let y = text_area.y + (app.cursor_line - app.scroll_y) as u16;
        let x =
            text_area.x + prefix_width as u16 + app.cursor_col.saturating_sub(app.scroll_x) as u16;
        if x < text_area.x + text_area.width && y < text_area.y + text_area.height {
            frame.set_cursor_position(Position::new(x, y));
        }
    }
}

pub(super) fn draw_plain_view(frame: &mut Frame<'_>, app: &mut Editor) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let width = area.width.max(1) as usize;
    let height = area.height.max(1) as usize;
    app.ensure_cursor_visible(height, width);

    let end = (app.scroll_y + height).min(app.line_count());
    let mut lines = Vec::with_capacity(height);
    for idx in app.scroll_y..end {
        lines.push(Line::from(line_text(&app.buffer, idx)));
    }
    while lines.len() < height {
        lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_lines(
    app: &Editor,
    syntax: &SyntaxHighlighter,
    text_height: usize,
    text_width: usize,
    prefix_width: usize,
) -> Vec<Line<'static>> {
    let mut highlighter = HighlightLines::new(syntax.syntax(), &syntax.theme);
    let end = (app.scroll_y + text_height).min(app.line_count());
    let mut output = Vec::with_capacity(text_height);
    let active_region = app.active_region();

    for idx in app.scroll_y..end {
        let line = line_text(&app.buffer, idx);
        let line_region = line_selection_range(app, idx, active_region.as_ref());
        let highlighted = highlighter
            .highlight_line(&line, &syntax.syntax_set)
            .unwrap_or_else(|_| vec![(syntect::highlighting::Style::default(), line.as_str())]);
        let mut spans = vec![Span::styled(
            format!("{:>width$} ", idx + 1, width = prefix_width - 1),
            Style::default().fg(Color::DarkGray),
        )];
        spans.extend(slice_highlighted_line(
            highlighted,
            app.scroll_x,
            text_width,
            line_region,
            app.commit_subject_limit_for_line(idx),
        ));
        output.push(Line::from(spans));
    }

    while output.len() < text_height {
        output.push(Line::from(vec![Span::styled(
            "~",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    output
}

fn slice_highlighted_line(
    highlighted: Vec<(syntect::highlighting::Style, &str)>,
    start: usize,
    width: usize,
    selection: Option<Range<usize>>,
    commit_subject_limit: Option<usize>,
) -> Vec<Span<'static>> {
    let end = start.saturating_add(width);
    let mut spans = Vec::new();
    let mut pos = 0usize;

    for (style, text) in highlighted {
        let base_style = syntect_style(style);
        for ch in text.chars() {
            if pos >= start && pos < end {
                let style = commit_subject_style(base_style, pos, commit_subject_limit);
                let style = if selection
                    .as_ref()
                    .is_some_and(|selection| selection.contains(&pos))
                {
                    style.bg(Color::DarkGray)
                } else {
                    style
                };
                spans.push(Span::styled(ch.to_string(), style));
            }
            pos += 1;
        }
        if pos >= end {
            break;
        }
    }

    spans
}

fn commit_subject_style(style: Style, pos: usize, limit: Option<usize>) -> Style {
    let Some(limit) = limit else {
        return style;
    };
    if pos < limit {
        style.fg(Color::Cyan)
    } else {
        style.fg(Color::Red).add_modifier(Modifier::BOLD)
    }
}

fn syntect_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::ITALIC)
    {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::UNDERLINE)
    {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_subject_style_marks_limit_and_overflow_differently() {
        let base = Style::default();
        let in_limit = commit_subject_style(base, 49, Some(50));
        let overflow = commit_subject_style(base, 50, Some(50));

        assert_eq!(in_limit.fg, Some(Color::Cyan));
        assert_eq!(overflow.fg, Some(Color::Red));
        assert!(overflow.add_modifier.contains(Modifier::BOLD));
    }
}
