// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Markdown (GFM) parser for the rendered [`Preview`](crate::Preview) (M4): a `.md` source becomes
//! styled display lines.
//!
//! [`layout`] parses GFM with `pulldown-cmark` and drives a [`LineBuilder`] — the shared typesetter in
//! [`crate::preview`] — emitting the Steelbore palette: headings bold (with a rule under `#`/`##`),
//! `code` in Steel Blue, links in Liquid Coolant underlined, blockquotes barred, checked task items in
//! Radium Green. Covers headings, word-wrapped paragraphs, strong/emphasis/inline-code/strikethrough/
//! links, bullet + ordered + task lists (nested), fenced/indented code blocks, blockquotes, thematic
//! breaks, image alt-text, and GFM tables as aligned columns.
//
// Rust guideline compliant 2026-05-18

use penumbra::Style;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::preview::{text_width, Line, LineBuilder, Palette};

/// Spaces of indent per list-nesting level.
const INDENT: usize = 2;
/// Spaces a fenced/indented code block is inset by.
const CODE_INDENT: usize = 2;

/// Lays a GFM `source` out into styled display lines wrapped to `width` columns, styled with `palette`.
pub(crate) fn layout(source: &str, width: usize, palette: &Palette) -> Vec<Line> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    let mut layouter = Layouter::new(width, palette);
    for event in Parser::new_ext(source, options) {
        layouter.event(&event);
    }
    layouter.builder.finish()
}

/// Walks `pulldown-cmark` events, driving a [`LineBuilder`].
struct Layouter<'a> {
    builder: LineBuilder<'a>,
    strong: u32,
    emphasis: u32,
    strike: u32,
    link: bool,
    heading_level: Option<usize>,
    in_code_block: bool,
    quote_depth: usize,
    /// Ordered-list counters (next number) or `None` for a bullet list, innermost last.
    list_stack: Vec<Option<u64>>,
    /// The GFM table currently being collected (cells gathered until `TagEnd::Table`, then rendered).
    table: Option<TableState>,
}

impl<'a> Layouter<'a> {
    fn new(width: usize, palette: &'a Palette) -> Self {
        Self {
            builder: LineBuilder::new(width, palette),
            strong: 0,
            emphasis: 0,
            strike: 0,
            link: false,
            heading_level: None,
            in_code_block: false,
            quote_depth: 0,
            list_stack: Vec::new(),
            table: None,
        }
    }

    fn event(&mut self, event: &Event<'_>) {
        if self.table.is_some() {
            self.table_event(event);
            return;
        }
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(*tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_text(text);
                } else {
                    let style = self.inline_style();
                    self.builder.push_span(text, style);
                }
            }
            Event::Code(text) => self.builder.push_span(text, self.builder.palette().code),
            Event::HardBreak => self.builder.end_line(),
            Event::Rule => {
                self.builder.blank();
                self.builder.rule();
                self.builder.blank();
            }
            Event::TaskListMarker(checked) => self.task_marker(*checked),
            Event::SoftBreak => self.builder.soft_break(),
            // HTML / footnotes / math are ignored in v1.
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => self.builder.blank(),
            Tag::Heading { level, .. } => {
                self.builder.blank();
                self.heading_level = Some(heading_level(*level));
            }
            Tag::Strong => self.strong += 1,
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } | Tag::Image { .. } => self.link = true,
            Tag::List(start) => {
                self.builder.blank();
                self.list_stack.push(*start);
            }
            Tag::Item => self.start_item(),
            Tag::CodeBlock(_) => {
                self.builder.blank();
                self.in_code_block = true;
                self.builder.set_indent(self.builder.indent() + CODE_INDENT);
            }
            Tag::BlockQuote(_) => {
                self.builder.end_line();
                self.quote_depth += 1;
                self.builder.set_quote_depth(self.quote_depth);
            }
            Tag::Table(_) => self.table = Some(TableState::default()),
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Item => self.builder.end_line(),
            TagEnd::Heading(level) => {
                self.builder.end_line();
                self.heading_level = None;
                if heading_level(level) <= 2 {
                    self.builder.rule();
                }
            }
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => self.link = false,
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.builder.end_line();
                }
            }
            TagEnd::CodeBlock => {
                self.builder.end_line();
                self.in_code_block = false;
                self.builder
                    .set_indent(self.builder.indent().saturating_sub(CODE_INDENT));
            }
            TagEnd::BlockQuote(_) => {
                self.builder.end_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.builder.set_quote_depth(self.quote_depth);
            }
            _ => {}
        }
    }

    /// Begins a list item: computes its marker (bullet or `N.`) and the hanging indent for wraps.
    fn start_item(&mut self) {
        self.builder.end_line();
        let base = self.list_stack.len().saturating_sub(1) * INDENT;
        let marker = match self.list_stack.last_mut() {
            Some(Some(number)) => {
                let marker = format!("{number}. ");
                *number += 1;
                marker
            }
            _ => "• ".to_owned(),
        };
        let marker_width = text_width(&marker);
        let mut prefix: Line = Vec::new();
        if base > 0 {
            prefix.push((" ".repeat(base), self.builder.palette().body));
        }
        prefix.push((marker, self.builder.palette().body));
        self.builder.set_marker(prefix);
        self.builder.set_indent(base + marker_width);
    }

    /// Replaces a list item's bullet with a task-list checkbox (`☐`/`☑`).
    fn task_marker(&mut self, checked: bool) {
        let (glyph, style) = if checked {
            ("☑ ".to_owned(), self.builder.palette().task_done)
        } else {
            ("☐ ".to_owned(), self.builder.palette().body)
        };
        let base = self.list_stack.len().saturating_sub(1) * INDENT;
        let mut prefix: Line = Vec::new();
        if base > 0 {
            prefix.push((" ".repeat(base), self.builder.palette().body));
        }
        prefix.push((glyph, style));
        self.builder.set_marker(prefix);
    }

    /// Appends fenced/indented code-block text, one source line per display line (no wrapping).
    fn code_text(&mut self, text: &str) {
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            // `append` starts the line with the code indent, so an empty `part` keeps a blank code line.
            self.builder.append(part, self.builder.palette().code);
            if parts.peek().is_some() {
                self.builder.end_line();
            }
        }
    }

    /// The active inline style from the strong/emphasis/link/heading state.
    fn inline_style(&self) -> Style {
        let palette = self.builder.palette();
        if self.heading_level.is_some() {
            return palette.heading;
        }
        let mut style = if self.link {
            palette.link
        } else {
            palette.body
        };
        if self.strong > 0 {
            style.attrs.bold = true;
        }
        if self.emphasis > 0 {
            style.attrs.italic = true;
        }
        style
    }

    /// Collects a GFM table's cells from its events; renders it when the table ends.
    fn table_event(&mut self, event: &Event<'_>) {
        if matches!(event, Event::End(TagEnd::Table)) {
            if let Some(table) = self.table.take() {
                self.render_table(&table);
            }
            return;
        }
        let Some(table) = self.table.as_mut() else {
            return;
        };
        match event {
            Event::Start(Tag::TableHead | Tag::TableRow) => table.row = Vec::new(),
            Event::Start(Tag::TableCell) => table.cell = String::new(),
            // Cells collect plain text (inline styling within a cell is dropped in v1).
            Event::Text(text) | Event::Code(text) => table.cell.push_str(text),
            Event::End(TagEnd::TableCell) => {
                let cell = std::mem::take(&mut table.cell);
                table.row.push(cell);
            }
            Event::End(TagEnd::TableHead) => {
                table.rows.push(std::mem::take(&mut table.row));
                table.head_rows = table.rows.len();
            }
            Event::End(TagEnd::TableRow) => table.rows.push(std::mem::take(&mut table.row)),
            _ => {}
        }
    }

    /// Renders a collected table as aligned columns separated by `│`, with a rule under the header.
    fn render_table(&mut self, table: &TableState) {
        crate::preview::render_table(&mut self.builder, &table.rows, table.head_rows);
    }
}

/// A GFM table collected from `pulldown-cmark` events: rows of plain-text cells (the header first).
#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    /// How many leading rows are header rows (for bold + the separator rule).
    head_rows: usize,
    row: Vec<String>,
    cell: String,
}

/// The 1-based level of a heading (`#` = 1 … `######` = 6).
fn heading_level(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::layout;
    use crate::preview::Palette;
    use penumbra::Theme;

    /// Renders `markdown` to plain strings (one per display line), dropping styles.
    fn rendered(markdown: &str, width: usize) -> Vec<String> {
        let palette = Palette::new(&Theme::steelbore());
        layout(markdown, width, &palette)
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(text, _)| text.as_str())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn a_heading_gets_a_rule_under_it() {
        let lines = rendered("# Title\n", 20);
        assert_eq!(lines[0], "Title");
        assert!(
            lines[1].chars().all(|c| c == '─'),
            "an H1 is underlined by a rule"
        );
    }

    #[test]
    fn bullet_ordered_and_task_lists_render_markers() {
        assert_eq!(rendered("- one\n- two\n", 20), ["• one", "• two"]);
        assert_eq!(rendered("1. a\n2. b\n", 20), ["1. a", "2. b"]);
        assert_eq!(
            rendered("- [ ] todo\n- [x] done\n", 20),
            ["☐ todo", "☑ done"]
        );
    }

    #[test]
    fn a_code_block_keeps_its_lines_verbatim_and_indented() {
        let lines = rendered("```\nlet x = 1;\n```\n", 40);
        assert!(
            lines.iter().any(|line| line == "  let x = 1;"),
            "code lines are inset by two spaces and not wrapped: {lines:?}"
        );
    }

    #[test]
    fn a_long_paragraph_wraps_at_the_width() {
        let lines = rendered("alpha beta gamma delta epsilon", 12);
        assert!(lines.len() > 1, "the paragraph wraps: {lines:?}");
    }

    #[test]
    fn inline_runs_keep_exact_spacing_around_punctuation() {
        // An inline run followed by punctuation has no spurious space; adjacent runs don't gain one.
        assert_eq!(rendered("see `code`.", 40), ["see code."]);
        assert_eq!(rendered("a`b`c", 40), ["abc"]);
        assert_eq!(rendered("**bold**, then more", 40), ["bold, then more"]);
    }

    #[test]
    fn a_gfm_table_renders_aligned_columns() {
        let lines = rendered("| Name | Qty |\n|------|-----|\n| mj | 1 |\n", 40);
        assert_eq!(
            lines[0], "Name │ Qty",
            "header cells padded to the column width"
        );
        assert!(
            lines[1].starts_with('─') && lines[1].contains('┼'),
            "a rule under the header"
        );
        assert_eq!(
            lines[2], "mj   │ 1",
            "body cells padded to align under the header"
        );
    }

    #[test]
    fn an_h1_span_is_bold_and_a_link_is_underlined() {
        let palette = Palette::new(&Theme::steelbore());
        assert!(
            layout("# Hi", 20, &palette)[0][0].1.attrs.bold,
            "heading text is bold"
        );
        let link = layout("[text](http://example.com)", 20, &palette);
        let span = &link[0][0];
        assert_eq!(span.0, "text", "link text is rendered (URL elided in v1)");
        assert!(span.1.attrs.underline, "link is underlined");
    }
}
