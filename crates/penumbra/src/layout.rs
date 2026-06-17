// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! [`Rect`] — a rectangular screen region, the unit the window layout is built from.
//!
//! A [`Buffer`](crate::Buffer) is one flat grid; widgets (the editor, the terminal panel, the
//! status bar) each draw into a sub-[`Rect`] of it, offsetting and clipping their writes to
//! that region. Splitting a `Rect` (e.g. [`Rect::split_bottom`]) is how the UI.md layout —
//! editor area over a bottom terminal panel over a status bar — is composed without any
//! widget needing to know the whole screen.

/// A rectangular region of a [`Buffer`](crate::Buffer), in cell coordinates.
///
/// `(x, y)` is the top-left corner; `width`/`height` are extents in cells. The region covers
/// columns `x..x + width` and rows `y..y + height`. All arithmetic saturates, so regions stay
/// in `u16` range and never wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    /// Left edge (column of the first cell).
    pub x: u16,
    /// Top edge (row of the first cell).
    pub y: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

impl Rect {
    /// Creates a region at `(x, y)` of `width × height` cells.
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The column just past the right edge (`x + width`, saturating).
    #[must_use]
    pub const fn right(&self) -> u16 {
        self.x.saturating_add(self.width)
    }

    /// The row just past the bottom edge (`y + height`, saturating).
    #[must_use]
    pub const fn bottom(&self) -> u16 {
        self.y.saturating_add(self.height)
    }

    /// `true` when the region has no cells (zero width or height).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// `true` when cell `(x, y)` lies inside the region.
    #[must_use]
    pub const fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Splits `rows` off the bottom, returning `(top, bottom)`.
    ///
    /// `bottom` is the last `rows` rows (clamped to this region's height); `top` is what
    /// remains above it. Asking for more rows than exist yields an empty `top`.
    #[must_use]
    pub const fn split_bottom(self, rows: u16) -> (Self, Self) {
        let rows = if rows > self.height {
            self.height
        } else {
            rows
        };
        let top_height = self.height - rows;
        let top = Self::new(self.x, self.y, self.width, top_height);
        let bottom = Self::new(self.x, self.y.saturating_add(top_height), self.width, rows);
        (top, bottom)
    }

    /// Splits the top `rows` off, returning `(top, bottom)` — the mirror of [`Self::split_bottom`].
    #[must_use]
    pub const fn split_top(self, rows: u16) -> (Self, Self) {
        let rows = if rows > self.height {
            self.height
        } else {
            rows
        };
        let top = Self::new(self.x, self.y, self.width, rows);
        let bottom = Self::new(
            self.x,
            self.y.saturating_add(rows),
            self.width,
            self.height - rows,
        );
        (top, bottom)
    }

    /// Splits the left `cols` off, returning `(left, right)`.
    ///
    /// `left` is the first `cols` columns (clamped to this region's width); `right` is what
    /// remains. Asking for more columns than exist yields an empty `right`.
    #[must_use]
    pub const fn split_left(self, cols: u16) -> (Self, Self) {
        let cols = if cols > self.width { self.width } else { cols };
        let left = Self::new(self.x, self.y, cols, self.height);
        let right = Self::new(
            self.x.saturating_add(cols),
            self.y,
            self.width - cols,
            self.height,
        );
        (left, right)
    }
}

#[cfg(test)]
mod tests {
    use super::Rect;

    #[test]
    fn edges_and_containment() {
        let rect = Rect::new(2, 3, 4, 5);
        assert_eq!(rect.right(), 6);
        assert_eq!(rect.bottom(), 8);
        assert!(rect.contains(2, 3));
        assert!(rect.contains(5, 7));
        assert!(!rect.contains(6, 7)); // just past the right edge
        assert!(!rect.contains(5, 8)); // just past the bottom edge
        assert!(!rect.contains(1, 3));
    }

    #[test]
    fn empty_when_zero_extent() {
        assert!(Rect::new(0, 0, 0, 4).is_empty());
        assert!(Rect::new(0, 0, 4, 0).is_empty());
        assert!(!Rect::new(0, 0, 1, 1).is_empty());
    }

    #[test]
    fn split_bottom_partitions_height() {
        let (top, bottom) = Rect::new(0, 0, 10, 20).split_bottom(6);
        assert_eq!(top, Rect::new(0, 0, 10, 14));
        assert_eq!(bottom, Rect::new(0, 14, 10, 6));
        // The two halves tile the original with no gap or overlap.
        assert_eq!(top.bottom(), bottom.y);
    }

    #[test]
    fn split_bottom_clamps_to_height() {
        let (top, bottom) = Rect::new(0, 0, 10, 4).split_bottom(9);
        assert!(top.is_empty());
        assert_eq!(bottom, Rect::new(0, 0, 10, 4));
    }

    #[test]
    fn split_top_is_the_mirror() {
        let (top, bottom) = Rect::new(1, 2, 8, 10).split_top(3);
        assert_eq!(top, Rect::new(1, 2, 8, 3));
        assert_eq!(bottom, Rect::new(1, 5, 8, 7));
    }

    #[test]
    fn split_left_partitions_width() {
        let (left, right) = Rect::new(0, 0, 30, 12).split_left(8);
        assert_eq!(left, Rect::new(0, 0, 8, 12));
        assert_eq!(right, Rect::new(8, 0, 22, 12));
        assert_eq!(left.right(), right.x);

        let (left, right) = Rect::new(0, 0, 4, 4).split_left(9);
        assert_eq!(left, Rect::new(0, 0, 4, 4));
        assert!(right.is_empty());
    }
}
