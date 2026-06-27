// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared rendered-document **preview** — a modal pane that shows a source document formatted
//! (M4). [`Preview`] holds the source + a [`PreviewKind`] and lays it out into styled display lines
//! for a [`penumbra`] [`Buffer`], mirroring the [`InfoReader`](crate::InfoReader): the host draws it
//! over the editor and scrolls it with the keyboard.
//!
//! The *typesetting* — word-wrapping styled runs into lines with indents, list markers, and quote
//! bars — lives in the format-agnostic [`LineBuilder`]; each format (Markdown via
//! [`crate::markdown`], Texinfo via [`crate::texinfo`]) is a thin parser that drives a `LineBuilder`.
//! Styling uses the Steelbore six-token palette (Standard §9).
//
// Rust guideline compliant 2026-05-18

use penumbra::{char_width, Buffer, Rect, Style, Theme};

/// One rendered display line: styled text runs, left to right.
pub(crate) type Line = Vec<(String, Style)>;

/// Which markup language a [`Preview`] renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    /// GitHub-Flavored Markdown (`.md`).
    Markdown,
    /// GNU Texinfo (`.texi`).
    Texinfo,
}

impl PreviewKind {
    /// A human label for the preview header.
    const fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Texinfo => "Texinfo",
        }
    }
}

/// A scrollable, styled rendering of a source document — the preview pane (M4).
///
/// Construct with [`Preview::new`], then call [`Preview::render`] each frame; the layout is recomputed
/// only when the pane width changes. Scroll with [`Preview::scroll_up`] / [`Preview::scroll_down`].
#[derive(Debug)]
pub struct Preview {
    source: String,
    title: String,
    kind: PreviewKind,
    /// Laid-out display lines for `width`; empty until the first [`Self::render`].
    lines: Vec<Line>,
    /// The content width `lines` were laid out for (`0` = not yet laid out).
    width: u16,
    scroll: usize,
}

impl Preview {
    /// Creates a preview of `source` (titled `title`, e.g. the file name) as `kind`. Laid out lazily
    /// on the first [`Self::render`], once the pane width is known.
    #[must_use]
    pub fn new(source: impl Into<String>, title: impl Into<String>, kind: PreviewKind) -> Self {
        Self {
            source: source.into(),
            title: title.into(),
            kind,
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
        let header_style = Style::new(theme.background, theme.accent); // inverse bar
        for x in header.x..header.right() {
            surface.set_char(x, header.y, ' ', header_style);
        }
        surface.set_str(
            header.x,
            header.y,
            &format!(
                " {} — {} preview · q to close",
                self.title,
                self.kind.label()
            ),
            header_style,
        );
        if body.is_empty() {
            return;
        }

        if self.width != body.width {
            let palette = Palette::new(theme);
            let columns = usize::from(body.width).max(1);
            self.lines = match self.kind {
                PreviewKind::Markdown => crate::markdown::layout(&self.source, columns, &palette),
                PreviewKind::Texinfo => crate::texinfo::layout(&self.source, columns, &palette),
            };
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

/// The Steelbore palette resolved into the styles a preview uses (Standard §9 six-token contract).
pub(crate) struct Palette {
    pub(crate) body: Style,
    pub(crate) code: Style,
    pub(crate) link: Style,
    pub(crate) heading: Style,
    pub(crate) rule: Style,
    pub(crate) quote: Style,
    pub(crate) task_done: Style,
}

impl Palette {
    pub(crate) fn new(theme: &Theme) -> Self {
        // Strong / emphasis aren't precomputed: a parser composes bold + italic onto whichever base
        // (body, link, …) is active, so they can nest and combine.
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

/// The format-agnostic typesetter: word-wraps styled runs into [`Line`]s within a width, honoring a
/// per-block continuation indent, a one-shot item marker, and blockquote bars. Parsers drive it with
/// [`push_text`](Self::push_text) / [`append`](Self::append) for content and the block setters for
/// structure, then call [`finish`](Self::finish).
pub(crate) struct LineBuilder<'a> {
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
    /// One-shot first-line prefix (e.g. a list-item marker), replacing the indent on the next line.
    pending_marker: Option<Line>,
    /// Whether the next placed word should be preceded by an inter-word space (carried across spans).
    space_pending: bool,
}

impl<'a> LineBuilder<'a> {
    pub(crate) fn new(width: usize, palette: &'a Palette) -> Self {
        Self {
            width: width.max(1),
            palette,
            lines: Vec::new(),
            line: Vec::new(),
            line_started: false,
            col: 0,
            prefix_width: 0,
            indent: 0,
            quote_depth: 0,
            pending_marker: None,
            space_pending: false,
        }
    }

    pub(crate) fn palette(&self) -> &Palette {
        self.palette
    }

    pub(crate) fn set_indent(&mut self, cols: usize) {
        self.indent = cols;
    }

    pub(crate) fn indent(&self) -> usize {
        self.indent
    }

    pub(crate) fn set_quote_depth(&mut self, depth: usize) {
        self.quote_depth = depth;
    }

    /// Sets the one-shot prefix for the next line (a list-item marker, including its leading spaces).
    pub(crate) fn set_marker(&mut self, prefix: Line) {
        self.pending_marker = Some(prefix);
    }

    /// Appends `text` onto the current line, word-wrapping at the width and **preserving exact
    /// spacing**: consecutive calls insert no space between them, so an inline run followed by
    /// punctuation (`@code{x}` then `.`) renders as `x.`, not `x .`. Whitespace within `text` is
    /// collapsed to single inter-word spaces (prose), carried across calls via `space_pending`.
    pub(crate) fn push_span(&mut self, text: &str, style: Style) {
        let mut word = String::new();
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !word.is_empty() {
                    self.emit_word(&word, style);
                    word.clear();
                }
                self.space_pending = true;
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            self.emit_word(&word, style);
        }
    }

    /// Places one whitespace-free `word`, honoring a pending inter-word space and wrapping at the width.
    fn emit_word(&mut self, word: &str, style: Style) {
        self.ensure_line();
        let word_width = text_width(word);
        let mid_line = self.col > self.prefix_width;
        let space = self.space_pending && mid_line;
        let advance = if space { word_width + 1 } else { word_width };
        if mid_line && self.col + advance > self.width {
            self.end_line();
            self.ensure_line();
        } else if space {
            self.append_raw(" ", self.palette.body);
        }
        self.space_pending = false;
        self.append_raw(word, style);
    }

    /// Records that the next word should be preceded by a space (e.g. a Markdown soft break).
    pub(crate) fn soft_break(&mut self) {
        self.space_pending = true;
    }

    /// Appends one styled run verbatim — no wrapping, no auto-spacing (for code lines and tables).
    pub(crate) fn append(&mut self, text: &str, style: Style) {
        self.space_pending = false;
        self.append_raw(text, style);
    }

    /// Pushes a run onto the current line (starting it if needed), advancing the column.
    fn append_raw(&mut self, text: &str, style: Style) {
        self.ensure_line();
        self.col += text_width(text);
        self.line.push((text.to_owned(), style));
    }

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
        self.space_pending = false; // a fresh line never owes a leading space
    }

    /// Ends the current line (if started), pushing it to the output.
    pub(crate) fn end_line(&mut self) {
        if self.line_started {
            self.lines.push(std::mem::take(&mut self.line));
            self.col = 0;
            self.prefix_width = 0;
            self.line_started = false;
        }
    }

    /// Emits a blank spacer line, collapsing consecutive blanks and skipping a leading one.
    pub(crate) fn blank(&mut self) {
        self.end_line();
        if self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(Vec::new());
        }
    }

    /// Emits a full-width horizontal rule.
    pub(crate) fn rule(&mut self) {
        self.end_line();
        self.lines
            .push(vec![("─".repeat(self.width), self.palette.rule)]);
    }

    /// Finishes layout, trimming trailing blank lines.
    pub(crate) fn finish(mut self) -> Vec<Line> {
        self.end_line();
        while self.lines.last().is_some_and(Vec::is_empty) {
            self.lines.pop();
        }
        self.lines
    }
}

/// The display width of `text` in terminal cells (wide glyphs count as two).
pub(crate) fn text_width(text: &str) -> usize {
    text.chars().map(|c| usize::from(char_width(c))).sum()
}

/// Renders `rows` of plain-text cells as aligned columns separated by `│`, with a `─┼─` rule under the
/// first `head_rows` header rows (header cells bold). Shared by the Markdown (GFM) and Texinfo
/// (`@multitable`) parsers. Columns are sized to their widest cell; an over-wide table is clipped by
/// the pane rather than wrapped (a follow-up).
pub(crate) fn render_table(builder: &mut LineBuilder<'_>, rows: &[Vec<String>], head_rows: usize) {
    if rows.is_empty() {
        return;
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0_usize; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(text_width(cell));
        }
    }
    let (heading, body, rule) = {
        let palette = builder.palette();
        (palette.heading, palette.body, palette.rule)
    };
    builder.blank();
    for (index, row) in rows.iter().enumerate() {
        let style = if index < head_rows { heading } else { body };
        for (column, &width) in widths.iter().enumerate() {
            if column > 0 {
                builder.append(" │ ", rule);
            }
            let cell = row.get(column).map_or("", String::as_str);
            let padding = if column + 1 == columns {
                String::new() // the last column needs no trailing padding
            } else {
                " ".repeat(width.saturating_sub(text_width(cell)))
            };
            builder.append(&format!("{cell}{padding}"), style);
        }
        builder.end_line();
        if index + 1 == head_rows {
            for (column, &width) in widths.iter().enumerate() {
                if column > 0 {
                    builder.append("─┼─", rule);
                }
                builder.append(&"─".repeat(width), rule);
            }
            builder.end_line();
        }
    }
    builder.blank();
}
