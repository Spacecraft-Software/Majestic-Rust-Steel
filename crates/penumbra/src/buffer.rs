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

use crate::layout::Rect;
use crate::theme::Style;

/// A single screen cell: a character and its [`Style`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The displayed character.
    pub symbol: char,
    /// The cell's colors and attributes.
    pub style: Style,
}

impl Cell {
    /// Creates a cell with `symbol` and `style`.
    #[must_use]
    pub const fn new(symbol: char, style: Style) -> Self {
        Self { symbol, style }
    }

    /// Creates a blank (space) cell in `style`.
    #[must_use]
    pub const fn blank(style: Style) -> Self {
        Self::new(' ', style)
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
    pub fn set_char(&mut self, x: u16, y: u16, symbol: char, style: Style) {
        self.set(x, y, Cell::new(symbol, style));
    }

    /// Writes `text` starting at `(x, y)`, clipping at the row's end.
    ///
    /// Returns the column just past the last character written.
    pub fn set_str(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        let mut col = x;
        for ch in text.chars() {
            if col >= self.width {
                break;
            }
            self.set_char(col, y, ch, style);
            col += 1;
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
    use super::Buffer;
    use crate::theme::Theme;

    fn style() -> crate::theme::Style {
        Theme::steelbore().base_style()
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
