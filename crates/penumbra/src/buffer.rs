// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The cell [`Buffer`] — a logical screen grid drawn into immediate-mode each frame.
//!
//! A [`Buffer`] is a `width × height` grid of [`Cell`]s. Each frame, the editor clears the
//! back buffer and redraws the whole logical frame into it; [`crate::render`] then diffs it
//! against the previously displayed frame and emits only the changed cells. This is the
//! msedit framebuffer-diff discipline — draw simply, pay only for what changed.
//!
//! Cells are one column wide in this M0 core (ASCII, box-drawing, and BMP-narrow text);
//! double-width/grapheme handling is layered on with `unicode-width`/`unicode-segmentation`.

use unicode_width::UnicodeWidthChar;

use crate::layout::Rect;
use crate::theme::Style;

/// The sentinel symbol marking a cell that is the second half of a double-width glyph to its
/// left. Such cells are never emitted by the renderer — the wide glyph already covers them.
const CONTINUATION: char = '\0';

/// The display width of `ch` in terminal cells: `2` for double-width glyphs (CJK, many emoji),
/// otherwise `1`. Combining and control characters collapse to a single cell in this model
/// (full grapheme/zero-width handling is layered on with `unicode-segmentation` later).
#[must_use]
pub fn char_width(ch: char) -> u16 {
    match UnicodeWidthChar::width(ch) {
        Some(2) => 2,
        _ => 1,
    }
}

/// A semantic icon a cell can carry for richer rendering. The TTY renderer ignores it and shows the
/// cell's [`symbol`](Cell::symbol); the GPU renderer (Nova, M4.6) draws the matching Material icon in
/// the cell instead. So a folder row reads `▸ src` in the terminal and shows a folder glyph in the
/// GUI — same layout either way (PRD §6.5 parity: the GUI is *richer*, not *different*).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// A collapsed directory.
    Folder,
    /// An expanded directory.
    FolderOpen,
    /// A generic file.
    File,
    /// A source-code file.
    Code,
}

/// A single screen cell: a character, its [`Style`], and an optional semantic [`Icon`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The displayed character.
    pub symbol: char,
    /// The cell's colors and attributes.
    pub style: Style,
    /// An optional semantic icon for richer (GPU) rendering; the TTY path ignores it. See [`Icon`].
    pub icon: Option<Icon>,
}

impl Cell {
    /// Creates a cell with `symbol` and `style` (no icon).
    #[must_use]
    pub const fn new(symbol: char, style: Style) -> Self {
        Self {
            symbol,
            style,
            icon: None,
        }
    }

    /// Creates a blank (space) cell in `style`.
    #[must_use]
    pub const fn blank(style: Style) -> Self {
        Self::new(' ', style)
    }

    /// Creates a continuation cell — the second column of a double-width glyph to its left.
    #[must_use]
    pub(crate) const fn continuation(style: Style) -> Self {
        Self::new(CONTINUATION, style)
    }

    /// Whether this cell is the trailing half of a double-width glyph (never emitted on its own).
    #[must_use]
    pub(crate) const fn is_continuation(&self) -> bool {
        matches!(self.symbol, CONTINUATION)
    }
}

/// A `width × height` grid of [`Cell`]s in row-major order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Buffer {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Buffer {
    /// Creates a buffer filled with blank cells in `fill`.
    #[must_use]
    pub fn new(width: u16, height: u16, fill: Style) -> Self {
        let count = usize::from(width) * usize::from(height);
        Self {
            width,
            height,
            cells: vec![Cell::blank(fill); count],
        }
    }

    /// The buffer width in columns.
    #[must_use]
    pub fn width(&self) -> u16 {
        self.width
    }

    /// The buffer height in rows.
    #[must_use]
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Resizes the buffer, clearing it to blank cells in `fill`.
    pub fn resize(&mut self, width: u16, height: u16, fill: Style) {
        self.width = width;
        self.height = height;
        let count = usize::from(width) * usize::from(height);
        self.cells.clear();
        self.cells.resize(count, Cell::blank(fill));
    }

    /// Resets every cell to a blank cell in `fill`.
    pub fn clear(&mut self, fill: Style) {
        let blank = Cell::blank(fill);
        for cell in &mut self.cells {
            cell.clone_from(&blank);
        }
    }

    /// The whole buffer as a [`Rect`] (origin `(0, 0)`).
    #[must_use]
    pub const fn area(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Fills `rect`'s cells with blanks in `fill` (clipped to the buffer).
    pub fn fill(&mut self, rect: Rect, fill: Style) {
        let blank = Cell::blank(fill);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                self.set(x, y, blank.clone());
            }
        }
    }

    /// Returns the cell at `(x, y)`, or `None` if out of bounds.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        self.index(x, y).map(|i| &self.cells[i])
    }

    /// Writes `cell` at `(x, y)`; out-of-bounds coordinates are ignored (clipped).
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if let Some(i) = self.index(x, y) {
            self.cells[i] = cell;
        }
    }

    /// Writes `symbol` in `style` at `(x, y)` (clipped).
    ///
    /// A double-width glyph also writes a continuation cell at `x + 1` so the column it covers is
    /// not redrawn or emitted separately. An incoming `NUL` is mapped to a space so it can never
    /// be mistaken for the internal continuation sentinel.
    pub fn set_char(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        let symbol = if symbol == CONTINUATION { ' ' } else { symbol };
        self.set(x, y, Cell::new(symbol, style));
        if char_width(symbol) == 2 {
            self.set(x.saturating_add(1), y, Cell::continuation(style));
        }
    }

    /// Sets (or clears) the semantic [`Icon`] of the cell at `(x, y)`, leaving its symbol and style
    /// untouched (clipped — out-of-bounds is a no-op). The TTY renderer ignores it; the GPU renderer
    /// draws the icon glyph in the cell. A front end sets this after writing the cell's text.
    pub fn set_icon(&mut self, x: u16, y: u16, icon: Option<Icon>) {
        if let Some(index) = self.index(x, y) {
            self.cells[index].icon = icon;
        }
    }

    /// Writes `text` starting at `(x, y)`, clipping at the row's end.
    ///
    /// Returns the column just past the last character written, advancing by each glyph's
    /// display width (so double-width glyphs occupy two columns).
    pub fn set_str(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        let mut col = x;
        for ch in text.chars() {
            if col >= self.width {
                break;
            }
            self.set_char(col, y, ch, style);
            col = col.saturating_add(char_width(ch));
        }
        col
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        (x < self.width && y < self.height)
            .then(|| usize::from(y) * usize::from(self.width) + usize::from(x))
    }
}

#[cfg(test)]
mod tests {
    use super::{char_width, Buffer};
    use crate::theme::Theme;

    fn style() -> crate::theme::Style {
        Theme::steelbore().base_style()
    }

    #[test]
    fn wide_glyph_occupies_two_columns() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('世'), 2);

        let mut buffer = Buffer::new(6, 1, style());
        // "a" (1) + "世" (2) + "b" (1) = next column 4.
        let next = buffer.set_str(0, 0, "a世b", style());
        assert_eq!(next, 4);
        assert_eq!(buffer.cell(0, 0).unwrap().symbol, 'a');
        assert_eq!(buffer.cell(1, 0).unwrap().symbol, '世');
        assert!(buffer.cell(2, 0).unwrap().is_continuation());
        assert_eq!(buffer.cell(3, 0).unwrap().symbol, 'b');
    }

    #[test]
    fn new_buffer_is_blank() {
        let buffer = Buffer::new(4, 2, style());
        assert_eq!(buffer.width(), 4);
        assert_eq!(buffer.height(), 2);
        assert_eq!(buffer.cell(0, 0).unwrap().symbol, ' ');
        assert_eq!(buffer.cell(3, 1).unwrap().symbol, ' ');
        assert!(buffer.cell(4, 0).is_none()); // out of bounds
        assert!(buffer.cell(0, 2).is_none());
    }

    #[test]
    fn set_str_writes_and_clips_at_row_end() {
        let mut buffer = Buffer::new(5, 1, style());
        let next = buffer.set_str(2, 0, "hello", style());
        assert_eq!(next, 5); // wrote "hel", clipped at width 5
        assert_eq!(buffer.cell(2, 0).unwrap().symbol, 'h');
        assert_eq!(buffer.cell(3, 0).unwrap().symbol, 'e');
        assert_eq!(buffer.cell(4, 0).unwrap().symbol, 'l');
    }

    #[test]
    fn out_of_bounds_set_is_ignored() {
        let mut buffer = Buffer::new(2, 2, style());
        buffer.set_char(9, 9, 'X', style()); // no panic, no effect
        assert_eq!(buffer.cell(0, 0).unwrap().symbol, ' ');
    }

    #[test]
    fn resize_reallocates_and_clears() {
        let mut buffer = Buffer::new(2, 2, style());
        buffer.set_char(0, 0, 'Z', style());
        buffer.resize(3, 1, style());
        assert_eq!(buffer.width(), 3);
        assert_eq!(buffer.height(), 1);
        assert_eq!(buffer.cell(0, 0).unwrap().symbol, ' ');
    }
}
