//! Markdown for assistant prose only. Layout counts cells before adding ANSI/OSC8.
use std::sync::Arc;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: Option<Arc<str>>,
}

#[derive(Clone)]
struct Cell {
    ch: char,
    style: Style,
}

struct Line {
    cells: Vec<Cell>,
    indent: usize,
    literal: bool,
}

#[derive(Default)]
struct Writer {
    lines: Vec<Line>,
    cells: Vec<Cell>,
    indent: usize,
    quote: usize,
    code: bool,
    hanging: usize,
}

impl Writer {
    fn text(&mut self, text: &str, style: &Style) {
        for ch in text.chars() {
            if ch == '\n' {
                self.newline(false);
                continue;
            }
            if ch.is_control() && ch != '\t' {
                continue; // Transcript text must not introduce terminal escape commands.
            }
            if self.cells.is_empty() {
                let prefix = format!(
                    "{}{}",
                    "│ ".repeat(self.quote),
                    " ".repeat(self.indent + usize::from(self.code) * 2)
                );
                self.hanging = self.quote * 2 + self.indent + usize::from(self.code) * 2;
                self.cells.extend(prefix.chars().map(|ch| Cell {
                    ch,
                    style: Style::default(),
                }));
            }
            if ch == '\t' {
                self.cells.extend((0..4).map(|_| Cell {
                    ch: ' ',
                    style: style.clone(),
                }));
            } else {
                self.cells.push(Cell {
                    ch,
                    style: style.clone(),
                });
            }
        }
    }

    fn newline(&mut self, blank: bool) {
        if !self.cells.is_empty() || (self.code && !blank) {
            self.lines.push(Line {
                cells: std::mem::take(&mut self.cells),
                indent: self.hanging,
                literal: self.code,
            });
        }
        if blank && self.lines.last().is_some_and(|line| !line.cells.is_empty()) {
            self.lines.push(Line {
                cells: Vec::new(),
                indent: 0,
                literal: false,
            });
        }
    }
}

fn safe_link(url: &str) -> Option<Arc<str>> {
    if (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(char::is_control)
    {
        Some(Arc::from(url))
    } else {
        None
    }
}

/// Render independent terminal rows: styling and hyperlinks close before each row ends.
pub(super) fn render(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut writer = Writer::default();
    let (mut bold, mut italic, mut strike) = (0usize, 0usize, 0usize);
    let mut link = None;
    let mut lists: Vec<Option<u64>> = Vec::new();
    for event in Parser::new_ext(
        text,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    ) {
        let style = Style {
            bold: bold > 0,
            italic: italic > 0,
            strike: strike > 0,
            code: writer.code,
            link: link.clone(),
        };
        match event {
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => strike += 1,
            Event::End(TagEnd::Strikethrough) => strike = strike.saturating_sub(1),
            Event::Start(Tag::Heading { .. }) => {
                writer.newline(false);
                bold += 1;
            }
            Event::End(TagEnd::Heading(_)) => {
                writer.newline(true);
                bold = bold.saturating_sub(1);
            }
            Event::End(TagEnd::Paragraph) => writer.newline(lists.is_empty()),
            Event::Start(Tag::CodeBlock(_)) => {
                writer.newline(false);
                writer.code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                writer.newline(true);
                writer.code = false;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                writer.newline(false);
                writer.quote += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                writer.newline(true);
                writer.quote = writer.quote.saturating_sub(1);
            }
            Event::Start(Tag::List(first)) => {
                writer.newline(false);
                lists.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                writer.newline(false);
                lists.pop();
                writer.indent = lists.len() * 2;
            }
            Event::Start(Tag::Item) => {
                writer.newline(false);
                writer.indent = lists.len().saturating_sub(1) * 2;
                let marker = match lists.last_mut() {
                    Some(Some(next)) => {
                        let s = format!("{next}. ");
                        *next += 1;
                        s
                    }
                    _ => "• ".to_string(),
                };
                writer.text(&marker, &Style::default());
                writer.hanging += marker.chars().count();
                writer.indent += marker.chars().count();
            }
            Event::End(TagEnd::Item) => writer.newline(false),
            Event::Start(Tag::Link { dest_url, .. }) => link = safe_link(&dest_url),
            Event::End(TagEnd::Link) => link = None,
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                writer.text(&text, &style)
            }
            Event::Code(text) => writer.text(
                &text,
                &Style {
                    code: true,
                    ..style
                },
            ),
            Event::SoftBreak => writer.text(" ", &style),
            Event::HardBreak => writer.newline(false),
            Event::Rule => {
                writer.newline(false);
                writer.text(&"─".repeat(width.min(40)), &Style::default());
                writer.newline(true);
            }
            Event::TaskListMarker(checked) => {
                writer.text(if checked { "[x] " } else { "[ ] " }, &style)
            }
            _ => {}
        }
    }
    writer.newline(false);
    while writer
        .lines
        .last()
        .is_some_and(|line| line.cells.is_empty())
    {
        writer.lines.pop();
    }
    writer
        .lines
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

fn wrap_line(line: &Line, width: usize) -> Vec<String> {
    if line.cells.is_empty() {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    let mut start = 0;
    let mut continuation = false;
    while start < line.cells.len() {
        let indent = if continuation {
            line.indent.min(width.saturating_sub(2))
        } else {
            0
        };
        let available = width - indent;
        let mut end = start;
        let mut used = 0;
        while end < line.cells.len() {
            let size = line.cells[end].ch.width().unwrap_or(0);
            if used + size > available {
                break;
            }
            used += size;
            end += 1;
        }
        if end == start {
            end += 1;
        } // A glyph wider than the viewport: always make progress.
        let mut next = end;
        if end < line.cells.len() && !line.literal {
            if let Some(space) = (start..end).rev().find(|&i| line.cells[i].ch == ' ')
                && line.cells[start..space].iter().any(|cell| cell.ch != ' ')
            {
                end = space;
                next = space + 1;
            }
            while next < line.cells.len() && line.cells[next].ch == ' ' {
                next += 1;
            }
        }
        result.push(format!(
            "{}{}",
            " ".repeat(indent),
            encode(&line.cells[start..end])
        ));
        start = next;
        continuation = true;
    }
    result
}

fn encode(cells: &[Cell]) -> String {
    let mut out = String::new();
    let mut previous = Style::default();
    for cell in cells {
        if cell.style != previous {
            if previous.link.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
            out.push_str("\x1b[0m");
            if cell.style.bold {
                out.push_str("\x1b[1m");
            }
            if cell.style.italic {
                out.push_str("\x1b[3m");
            }
            if cell.style.strike {
                out.push_str("\x1b[9m");
            }
            if cell.style.code {
                out.push_str("\x1b[36m");
            }
            if let Some(url) = &cell.style.link {
                out.push_str(&format!("\x1b[4m\x1b]8;;{url}\x1b\\"));
            }
            previous = cell.style.clone();
        }
        out.push(cell.ch);
    }
    if previous.link.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
    }
    out.push_str("\x1b[0m");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_styles_and_safe_links_survive_wrapping() {
        let rows = render(
            "# Heading\n\n**bold** and *italic* [docs](https://example.com/very-long-path)",
            18,
        );
        let output = rows.join("\n");
        assert!(output.contains("\x1b[1mHeading"));
        assert!(output.contains("\x1b[1mbold"));
        assert!(output.contains("\x1b[3mitalic"));
        assert!(output.contains("\x1b]8;;https://example.com/very-long-path\x1b\\docs"));
        assert!(!output.contains("**"));
        assert!(
            rows.iter()
                .all(|row| row.ends_with("\x1b[0m") || row.is_empty())
        );
    }

    #[test]
    fn code_preserves_markdown_and_controls_cannot_escape() {
        let output = render("```rust\nlet s = \"**literal**\";\n```\n\n`*code*` [unsafe](javascript:alert)\n\n\u{1b}[2J", 80).join("\n");
        assert!(output.contains("**literal**"));
        assert!(output.contains("*code*"));
        assert!(!output.contains("\x1b]8;;javascript:"));
        assert!(!output.contains("\x1b[2J"));
    }

    #[test]
    fn unicode_and_list_continuations_fit_cell_width() {
        let rows = render("- **日本語** item with a long explanation\n- next item", 14);
        let mut term = alacritty_terminal::term::Term::new(
            alacritty_terminal::term::Config::default(),
            &crate::pane::state::PaneTermSize {
                columns: 14,
                screen_lines: 30,
            },
            crate::pane::PaneListener::new(),
        );
        let mut processor = alacritty_terminal::vte::ansi::Processor::<
            alacritty_terminal::vte::ansi::StdSyncHandler,
        >::new();
        processor.advance(&mut term, rows.join("\r\n").as_bytes());
        use alacritty_terminal::grid::Dimensions;
        assert_eq!(term.grid().history_size(), 0);
        assert_eq!(term.grid().cursor.point.line.0 as usize, rows.len() - 1);
        assert!(rows.iter().skip(1).any(|row| row.starts_with("  ")));
    }
}
