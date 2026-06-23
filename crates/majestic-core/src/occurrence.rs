// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Symbol occurrences for document highlighting (PRD #1 §6.9; LSP `textDocument/documentHighlight`).
//!
//! Each [`Occurrence`] is a **byte range** of one use of the symbol under the cursor in the current
//! buffer; the editor tints them all so every place a name appears is visible at a glance, refreshed
//! as the cursor moves. [`Occurrence::write`] marks a write/definition use (tinted a touch more
//! strongly than a read). Editor-facing byte ranges — the LSP layer converts server positions to
//! byte offsets before they reach here, mirroring [`Diagnostic`](crate::Diagnostic).

use std::ops::Range;

/// One occurrence of the symbol under the cursor, as a byte range into the buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// The byte range of this occurrence within the buffer.
    pub range: Range<usize>,
    /// Whether this is a write/definition occurrence (else a read). The renderer marks writes a
    /// little more strongly so the definition stands out from its uses.
    pub write: bool,
}

impl Occurrence {
    /// Creates an occurrence over `range` (a `write` use when `write` is true).
    #[must_use]
    pub fn new(range: Range<usize>, write: bool) -> Self {
        Self { range, write }
    }

    /// Whether byte `offset` falls within this occurrence's span (a zero-width span still covers its
    /// single start position).
    #[must_use]
    pub fn covers(&self, offset: usize) -> bool {
        if self.range.start == self.range.end {
            offset == self.range.start
        } else {
            self.range.contains(&offset)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Occurrence;

    #[test]
    fn covers_the_half_open_span() {
        let occ = Occurrence::new(4..7, true);
        assert!(occ.write);
        assert!(!occ.covers(3));
        assert!(occ.covers(4));
        assert!(occ.covers(6));
        assert!(!occ.covers(7)); // end-exclusive
    }

    #[test]
    fn zero_width_span_covers_its_start() {
        let occ = Occurrence::new(5..5, false);
        assert!(occ.covers(5));
        assert!(!occ.covers(4));
        assert!(!occ.covers(6));
    }
}
