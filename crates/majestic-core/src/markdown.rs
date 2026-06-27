// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! A scrollable, styled **Markdown (GFM) preview** — the rendered view for `.md` buffers (M4).
//!
//! [`MarkdownPreview`] parses GFM with `pulldown-cmark` and lays it out into styled display lines for
//! a [`penumbra`] [`Buffer`], mirroring the [`InfoReader`](crate::InfoReader): a modal pane the host
//! draws over the editor, scrolled with the keyboard. The styling uses the Steelbore six-token palette
//! (Standard §9): headings bold (with a rule under `#`/`##`), `code` in Steel Blue, links in Liquid
//! Coolant underlined, blockquotes barred in Steel Blue, checked task items in Radium Green.
//!
//! Covered: headings, paragraphs (word-wrapped), strong/emphasis/inline-code/strikethrough/links,
//! bullet + ordered + task lists (nested), fenced/indented code blocks, blockquotes, thematic breaks,
//! and image alt-text. Tables render a placeholder for now (a clearly-marked follow-up).
//
// Rust guideline compliant 2026-05-18

use penumbra::{char_width, Buffer, Rect, Style, Theme};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Spaces of indent per list-nesting level.
const INDENT: usize = 2;
/// Spaces a fenced/indented code block is inset by.
const CODE_INDENT: usize = 2;

/// One rendered display line: styled text runs, left to right.
type Line = Vec<(String, Style)>;

/// A scrollable, styled rendering of a Markdown (GFM) document — the preview pane (M4).
///
/// Construct with [`MarkdownPreview::new`], then call [`MarkdownPreview::render`] each frame; the
/// layout is recomputed only when the pane width changes. Scroll with [`MarkdownPreview::scroll_up`]
/// / [`MarkdownPreview::scroll_down`].
#[derive(Debug)]
pub struct MarkdownPreview {
    source: String,
    title: String,
    /// Laid-out display lines for `width`; empty until the first [`Self::render`].
    lines: Vec<Line>,
    /// The content width `lines` were laid out for (`0` = not yet laid out).
    width: u16,
    scroll: usize,
}

impl MarkdownPreview {
    /// Creates a preview of `source` (GFM) titled `title` (e.g. the file name). The document is laid
    /// out lazily on the first [`Self::render`], once the pane width is known.
    #[must_use]
    pub fn new(source: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            title: title.into(),
            lines: Vec::new(),
            width: 0,
            scroll: 0,
        }
    }

    /// Scrolls down by `rows` (clamped to the document end at render time).
    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows);
    }

    /// Scrolls up by `rows`.
    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    /// Renders the header and the visible window into `area`, re-laying-out if the width changed.
    pub fn render(&mut self, surface: &mut Buffer, area: Rect, theme: &Theme) {
        if area.is_empty() {
            return;
        }
        let (header, body) = area.split_top(1);
        let header_style = Style::new(theme.background, theme.accent); // inverse bar (Void Navy on Steel Blue)
        for x in header.x..header.right() {
            surface.set_char(x, header.y, ' ', header_style);
        }
        surface.set_str(
            header.x,
            header.y,
            &format!(" {} — Markdown preview · q to close", self.title),
            header_style,
        );
        if body.is_empty() {
            return;
        }

        if self.width != body.width {
            self.lines = layout(&self.source, usize::from(body.width).max(1), theme);
            self.width = body.width;
        }
        let max_scroll = self.lines.len().saturating_sub(usize::from(body.height));
        self.scroll = self.scroll.min(max_scroll);

        let blank = Style::new(theme.foreground, theme.background);
        for row in 0..body.height {
            let y = body.y + row;
            for x in body.x..body.right() {
                surface.set_char(x, y, ' ', blank);
            }
            let Some(line) = self.lines.get(self.scroll + usize::from(row)) else {
                continue;
            };
            let mut col = body.x;
            for (text, style) in line {
                if col >= body.right() {
                    break;
                }
                col = surface.set_str(col, y, text, *style);
            }
        }
    }
}

/// Lays `source` out into styled display lines wrapped to `width` columns, styled with `theme`.
fn layout(source: &str, width: usize, theme: &Theme) -> Vec<Line> {
    let palette = Palette::new(theme);
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    let mut layouter = Layouter::new(width.max(1), &palette);
    for event in Parser::new_ext(source, options) {
        layouter.event(event);
    }
    layouter.finish()
}

/// The Steelbore palette resolved into the styles the preview uses (Standard §9 six-token contract).
struct Palette {
    body: Style,
    code: Style,
    link: Style,
    heading: Style,
    rule: Style,
    quote: Style,
    task_done: Style,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        // Strong / emphasis aren't precomputed: the inline style composes bold + italic onto whichever
        // base (body, link, …) is active, so they can nest and combine.
        let body = Style::new(theme.foreground, theme.background);
        let mut link = Style::new(theme.info, theme.background); // Liquid Coolant — links
        link.attrs.underline = true;
        Self {
            body,
            code: Style::new(theme.accent, theme.background), // Steel Blue — code
            link,
            heading: body.bold(),
            rule: Style::new(theme.accent, theme.background),
            quote: Style::new(theme.accent, theme.background),
            task_done: Style::new(theme.success, theme.background), // Radium Green — checked
        }
    }
}

/// Walks `pulldown-cmark` events, building wrapped, styled [`Line`]s.
struct Layouter<'a> {
    width: usize,
    palette: &'a Palette,
    lines: Vec<Line>,
    line: Line,
    line_started: bool,
    col: usize,
    /// Width of the current line's prefix (quote bars + indent/marker) — the wrap floor.
    prefix_width: usize,
    /// Continuation indent (spaces) for the current block.
    indent: usize,
    quote_depth: usize,
    /// One-shot first-line prefix for a list item (its leading spaces + marker).
    pending_marker: Option<Line>,
    strong: u32,
    emphasis: u32,
    strike: u32,
    link: bool,
    heading_level: Option<usize>,
    in_code_block: bool,
    /// Ordered-list counters (next number) or `None` for a bullet list, innermost last.
    list_stack: Vec<Option<u64>>,
    /// Table-nesting depth; while `> 0`, content is skipped (a placeholder was already emitted).
    table_depth: u32,
}

impl<'a> Layouter<'a> {
    fn new(width: usize, palette: &'a Palette) -> Self {
        Self {
            width,
            palette,
            lines: Vec::new(),
            line: Vec::new(),
            line_started: false,
            col: 0,
            prefix_width: 0,
            indent: 0,
            quote_depth: 0,
            pending_marker: None,
            strong: 0,
            emphasis: 0,
            strike: 0,
            link: false,
            heading_level: None,
            in_code_block: false,
            list_stack: Vec::new(),
            table_depth: 0,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if self.table_depth > 0 {
            // v1: skip the table body — the placeholder was emitted at the table's start.
            match event {
                Event::Start(Tag::Table(_)) => self.table_depth += 1,
                Event::End(TagEnd::Table) => self.table_depth -= 1,
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start_tag(&tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_text(&text);
                } else {
                    let style = self.inline_style();
                    self.push_text(&text, style);
                }
            }
            Event::Code(text) => self.push_text(&text, self.palette.code),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.blank();
                self.rule();
                self.blank();
            }
            Event::TaskListMarker(checked) => self.task_marker(checked),
            // SoftBreak is handled by inter-word spacing; HTML/footnotes/math are ignored in v1.
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => self.blank(),
            Tag::Heading { level, .. } => {
                self.blank();
                self.heading_level = Some(heading_level(*level));
            }
            Tag::Strong => self.strong += 1,
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { .. } | Tag::Image { .. } => self.link = true,
            Tag::List(start) => {
                self.blank();
                self.list_stack.push(*start);
            }
            Tag::Item => self.start_item(),
            Tag::CodeBlock(_) => {
                self.blank();
                self.in_code_block = true;
                self.indent += CODE_INDENT;
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote_depth += 1;
            }
            Tag::Table(_) => {
                self.blank();
                self.flush();
                self.lines
                    .push(vec![("[table — rich rendering pending]".to_owned(), self.palette.quote)]);
                self.table_depth = 1;
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Item => self.flush(),
            TagEnd::Heading(level) => {
                self.flush();
                self.heading_level = None;
                if heading_level(level) <= 2 {
                    self.rule();
                }
            }
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => self.link = false,
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.flush();
                }
            }
            TagEnd::CodeBlock => {
                self.flush();
                self.in_code_block = false;
                self.indent = self.indent.saturating_sub(CODE_INDENT);
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    /// Begins a list item: computes its marker (bullet or `N.`) and the hanging indent for wraps.
    fn start_item(&mut self) {
        self.flush();
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
            prefix.push((" ".repeat(base), self.palette.body));
        }
        prefix.push((marker, self.palette.body));
        self.pending_marker = Some(prefix);
        self.indent = base + marker_width;
    }

    /// Replaces a list item's bullet with a task-list checkbox (`☐`/`☑`).
    fn task_marker(&mut self, checked: bool) {
        let (glyph, style) = if checked {
            ("☑ ".to_owned(), self.palette.task_done)
        } else {
            ("☐ ".to_owned(), self.palette.body)
        };
        if let Some(prefix) = self.pending_marker.as_mut() {
            if let Some(last) = prefix.last_mut() {
                *last = (glyph, style);
            }
        }
    }

    /// Appends fenced/indented code-block text, one source line per display line (no wrapping).
    fn code_text(&mut self, text: &str) {
        let mut parts = text.split('\n').peekable();
        while let Some(part) = parts.next() {
            self.ensure_line();
            if !part.is_empty() {
                self.col += text_width(part);
                self.line.push((part.to_owned(), self.palette.code));
            }
            if parts.peek().is_some() {
                self.flush();
            }
        }
    }

    /// The active inline style from the strong/emphasis/link/heading state.
    fn inline_style(&self) -> Style {
        if self.heading_level.is_some() {
            return self.palette.heading;
        }
        let mut style = if self.link {
            self.palette.link
        } else {
            self.palette.body
        };
        if self.strong > 0 {
            style.attrs.bold = true;
        }
        if self.emphasis > 0 {
            style.attrs.italic = true;
        }
        style
    }

    /// Word-wraps `text` (styled) onto the current line, breaking at the pane width.
    fn push_text(&mut self, text: &str, style: Style) {
        for word in text.split_whitespace() {
            self.ensure_line();
            let word_width = text_width(word);
            let mid_line = self.col > self.prefix_width;
            if mid_line && self.col + 1 + word_width > self.width {
                self.flush();
                self.ensure_line();
            } else if mid_line {
                self.append(" ", self.palette.body);
            }
            self.append(word, style);
        }
    }

    /// Appends one styled run to the current line (starting a line if needed).
    fn append(&mut self, text: &str, style: Style) {
        self.ensure_line();
        self.col += text_width(text);
        self.line.push((text.to_owned(), style));
    }

    /// Starts the current line with its prefix (quote bars, then the item marker or indent) if it
    /// has not been started yet.
    fn ensure_line(&mut self) {
        if self.line_started {
            return;
        }
        self.line = Vec::new();
        self.col = 0;
        for _ in 0..self.quote_depth {
            self.col += 2;
            self.line.push(("│ ".to_owned(), self.palette.quote));
        }
        if let Some(prefix) = self.pending_marker.take() {
            for (text, style) in prefix {
                self.col += text_width(&text);
                self.line.push((text, style));
            }
        } else if self.indent > 0 {
            self.col += self.indent;
            self.line.push((" ".repeat(self.indent), self.palette.body));
        }
        self.prefix_width = self.col;
        self.line_started = true;
    }

    /// Ends the current line (if started), pushing it to the output.
    fn flush(&mut self) {
        if self.line_started {
            self.lines.push(std::mem::take(&mut self.line));
            self.col = 0;
            self.prefix_width = 0;
            self.line_started = false;
        }
    }

    /// Emits a blank spacer line, collapsing consecutive blanks and skipping a leading one.
    fn blank(&mut self) {
        self.flush();
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(Vec::new());
        }
    }

    /// Emits a full-width horizontal rule.
    fn rule(&mut self) {
        self.flush();
        self.lines
            .push(vec![("─".repeat(self.width), self.palette.rule)]);
    }

    /// Finishes layout, trimming trailing blank lines.
    fn finish(mut self) -> Vec<Line> {
        self.flush();
        while self.lines.last().is_some_and(Vec::is_empty) {
            self.lines.pop();
        }
        self.lines
    }
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

/// The display width of `text` in terminal cells (wide glyphs count as two).
fn text_width(text: &str) -> usize {
    text.chars().map(|c| usize::from(char_width(c))).sum()
}

#[cfg(test)]
mod tests {
    use super::{layout, text_width};
    use penumbra::Theme;

    /// Renders `markdown` to plain strings (one per display line), dropping styles.
    fn rendered(markdown: &str, width: usize) -> Vec<String> {
        let theme = Theme::steelbore();
        layout(markdown, width, &theme)
            .iter()
            .map(|line| line.iter().map(|(text, _)| text.as_str()).collect::<String>())
            .collect()
    }

    #[test]
    fn a_heading_gets_a_rule_under_it() {
        let lines = rendered("# Title\n", 20);
        assert_eq!(lines[0], "Title");
        assert!(lines[1].chars().all(|c| c == '─'), "an H1 is underlined by a rule");
    }

    #[test]
    fn bullet_ordered_and_task_lists_render_markers() {
        let bullets = rendered("- one\n- two\n", 20);
        assert_eq!(bullets, ["• one", "• two"]);
        let ordered = rendered("1. a\n2. b\n", 20);
        assert_eq!(ordered, ["1. a", "2. b"]);
        let tasks = rendered("- [ ] todo\n- [x] done\n", 20);
        assert_eq!(tasks, ["☐ todo", "☑ done"]);
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
        assert!(
            lines.iter().all(|line| text_width(line) <= 12),
            "no wrapped line exceeds the width: {lines:?}"
        );
    }

    #[test]
    fn an_h1_span_is_bold_and_a_link_is_underlined() {
        let theme = Theme::steelbore();
        let heading = layout("# Hi", 20, &theme);
        assert!(heading[0][0].1.attrs.bold, "heading text is bold");
        let link = layout("[text](http://example.com)", 20, &theme);
        let span = &link[0][0];
        assert_eq!(span.0, "text", "link text is rendered (URL elided in v1)");
        assert!(span.1.attrs.underline, "link is underlined");
        assert_eq!(span.1.fg, theme.info, "link uses the Liquid Coolant token");
    }
}
