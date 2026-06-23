// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Foldable line ranges (LSP `textDocument/foldingRange`).
//!
//! A [`FoldRange`] marks a region (e.g. a function body, a block, an import group) that can be
//! collapsed. The [`Editor`](crate::Editor) keeps the ranges the server reported plus which ones are
//! currently *collapsed*; when a range is collapsed, its interior lines are hidden from the render
//! and the viewport walks only the visible lines. The header line stays visible with a `⋯` marker.

/// A foldable region: the header line (which stays visible) through the last line of the region.
/// Both are 0-based and inclusive; a range is foldable only when it spans more than one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRange {
    /// The header line — visible even when folded, carrying the fold marker.
    pub start: usize,
    /// The last line of the region — hidden (with everything between it and the header) when folded.
    pub end: usize,
}

impl FoldRange {
    /// Creates a fold range spanning `start..=end` lines.
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Whether the range actually spans more than one line (a 1-line "range" is not foldable).
    #[must_use]
    pub fn is_foldable(&self) -> bool {
        self.end > self.start
    }

    /// Whether `line` is in the collapsible interior (`start < line <= end`) — the part hidden when
    /// this range is folded. The header (`start`) is never hidden.
    #[must_use]
    pub fn hides(&self, line: usize) -> bool {
        line > self.start && line <= self.end
    }
}
