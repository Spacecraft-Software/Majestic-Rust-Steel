// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Aggregated text metrics ([`Summary`]) and editor coordinates ([`Point`]).
//!
//! A [`Summary`] is the monoid the rope aggregates at every tree node: the byte,
//! `char`, and newline counts over a span of UTF-8 text. Because summaries combine
//! associatively, the rope answers length queries and coordinate conversions in
//! `O(log n)` by descending the tree and adding the summaries it skips. A [`Point`]
//! is a zero-based `(row, column)` editor position.

use std::ops::{Add, AddAssign};

/// Aggregated metrics over a span of UTF-8 text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Number of UTF-8 bytes.
    pub bytes: usize,
    /// Number of Unicode scalar values (`char`s).
    pub chars: usize,
    /// Number of newline (`\n`) bytes.
    pub newlines: usize,
}

impl Summary {
    /// The empty summary (all zero).
    pub const ZERO: Self = Self {
        bytes: 0,
        chars: 0,
        newlines: 0,
    };

    /// Computes the summary of a string slice in one pass.
    #[must_use]
    pub fn of(text: &str) -> Self {
        let mut chars = 0;
        let mut newlines = 0;
        for ch in text.chars() {
            chars += 1;
            if ch == '\n' {
                newlines += 1;
            }
        }
        Self {
            bytes: text.len(),
            chars,
            newlines,
        }
    }
}

impl Add for Summary {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            bytes: self.bytes + rhs.bytes,
            chars: self.chars + rhs.chars,
            newlines: self.newlines + rhs.newlines,
        }
    }
}

impl AddAssign for Summary {
    fn add_assign(&mut self, rhs: Self) {
        self.bytes += rhs.bytes;
        self.chars += rhs.chars;
        self.newlines += rhs.newlines;
    }
}

/// A zero-based `(row, column)` position; `column` is a byte offset within the row.
///
/// `row` counts preceding newlines. `column` is measured in bytes from the start of
/// the line (not graphemes or `char`s — grapheme-aware motion is layered on top later).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    /// Zero-based line index.
    pub row: usize,
    /// Byte offset from the start of the line.
    pub column: usize,
}

impl Point {
    /// Creates a point at `row` and `column`.
    #[must_use]
    pub const fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}
