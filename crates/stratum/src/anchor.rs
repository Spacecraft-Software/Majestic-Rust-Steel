// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Position [`Anchor`]s that survive edits, and the [`Edit`] deltas that move them.
//!
//! An anchor is a logical position that keeps pointing at the same place as text is
//! inserted and deleted around it (marker semantics) — used for cursors, selections,
//! marks, diagnostics, and the regions Seraph marks pending. Instead of a fixed byte
//! offset, an anchor carries a [`Bias`] deciding which way it leans when an edit lands
//! exactly at its position, and is carried forward with [`Anchor::rebase`].
//! [`Rope::edit`](crate::Rope::edit) returns the [`Edit`] describing a change precisely so
//! callers can rebase their anchors through it.
//!
//! # Examples
//! ```
//! use stratum::{Anchor, Bias, Rope};
//!
//! let rope = Rope::from("abcXdef");
//! let anchor = Anchor::new(3, Bias::Right); // sits just before 'X'
//! let (rope, edit) = rope.edit(0..0, "** "); // insert at the very start
//! let anchor = anchor.rebase(&edit);
//! assert_eq!(rope.slice(anchor.offset()..anchor.offset() + 1), "X");
//! ```

/// Which way an [`Anchor`] leans when an edit occurs exactly at its position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bias {
    /// Stay before text inserted at the anchor (the anchor does not move).
    Left,
    /// Move after text inserted at the anchor (the anchor follows the new text).
    Right,
}

/// A single replacement delta: `old_len` bytes at `start` became `new_len` bytes.
///
/// Returned by [`Rope::edit`](crate::Rope::edit) and consumed by [`Anchor::rebase`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Byte offset where the replaced range began.
    pub start: usize,
    /// Number of bytes removed.
    pub old_len: usize,
    /// Number of bytes inserted in their place.
    pub new_len: usize,
}

impl Edit {
    /// Creates an edit replacing `old_len` bytes at `start` with `new_len` bytes.
    #[must_use]
    pub const fn new(start: usize, old_len: usize, new_len: usize) -> Self {
        Self {
            start,
            old_len,
            new_len,
        }
    }
}

/// A logical position that survives edits, leaning [`Left`](Bias::Left) or [`Right`](Bias::Right).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    offset: usize,
    bias: Bias,
}

impl Anchor {
    /// Creates an anchor at byte `offset` with the given `bias`.
    #[must_use]
    pub const fn new(offset: usize, bias: Bias) -> Self {
        Self { offset, bias }
    }

    /// The anchor's current byte offset.
    #[must_use]
    pub const fn offset(self) -> usize {
        self.offset
    }

    /// The anchor's bias.
    #[must_use]
    pub const fn bias(self) -> Bias {
        self.bias
    }

    /// Returns the anchor moved through `edit`, keeping it at the same logical position.
    ///
    /// A position strictly before the edit is unchanged; one strictly after shifts by the
    /// edit's length delta; one inside the replaced range collapses to the start
    /// ([`Left`](Bias::Left)) or to the end of the inserted text ([`Right`](Bias::Right)).
    #[must_use]
    pub fn rebase(self, edit: &Edit) -> Self {
        let from = edit.start;
        let to = edit.start + edit.old_len;
        let offset = if self.offset < from {
            self.offset
        } else if self.offset > to {
            self.offset - edit.old_len + edit.new_len
        } else {
            match self.bias {
                Bias::Left => from,
                Bias::Right => from + edit.new_len,
            }
        };
        Self {
            offset,
            bias: self.bias,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Anchor, Bias, Edit};
    use crate::Rope;

    #[test]
    fn bias_decides_insertion_at_anchor() {
        let insert = Edit::new(5, 0, 3); // insert 3 bytes at offset 5
        assert_eq!(Anchor::new(5, Bias::Left).rebase(&insert).offset(), 5);
        assert_eq!(Anchor::new(5, Bias::Right).rebase(&insert).offset(), 8);
        assert_eq!(Anchor::new(4, Bias::Right).rebase(&insert).offset(), 4); // before
        assert_eq!(Anchor::new(6, Bias::Left).rebase(&insert).offset(), 9); // after, +3
    }

    #[test]
    fn deletion_collapses_interior_anchor() {
        let delete = Edit::new(2, 4, 0); // delete [2, 6)
        assert_eq!(Anchor::new(4, Bias::Left).rebase(&delete).offset(), 2);
        assert_eq!(Anchor::new(4, Bias::Right).rebase(&delete).offset(), 2); // new_len 0
        assert_eq!(Anchor::new(1, Bias::Left).rebase(&delete).offset(), 1); // before
        assert_eq!(Anchor::new(7, Bias::Left).rebase(&delete).offset(), 3); // after, -4
    }

    #[test]
    fn replacement_collapses_by_bias() {
        let replace = Edit::new(2, 4, 3); // replace [2, 6) with 3 bytes
        assert_eq!(Anchor::new(4, Bias::Left).rebase(&replace).offset(), 2);
        assert_eq!(Anchor::new(4, Bias::Right).rebase(&replace).offset(), 5); // 2 + 3
        assert_eq!(Anchor::new(6, Bias::Right).rebase(&replace).offset(), 5); // pos == to
        assert_eq!(Anchor::new(8, Bias::Left).rebase(&replace).offset(), 7); // after: 8-4+3
    }

    /// Tiny deterministic PRNG (xorshift64*), mirroring the rope test harness.
    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                return 0;
            }
            usize::try_from(self.next_u64() % n as u64).unwrap_or(0)
        }
    }

    /// A right-biased anchor on a unique sentinel must track it through edits on either
    /// side that never delete it — checked against the model's true sentinel offset.
    #[test]
    fn anchor_tracks_sentinel_through_random_edits() {
        let mut rng = Rng(0x00A1_1CE5);
        let mut model = String::from("AAAAA\u{1}BBBBB");
        let mut rope = Rope::from(model.as_str());
        let mut anchor = Anchor::new(model.find('\u{1}').unwrap(), Bias::Right);

        for _ in 0..2000 {
            let sentinel = model.find('\u{1}').unwrap();
            let (range, insert) = if rng.below(2) == 0 {
                // Edit within the prefix [0, sentinel]; never deletes the sentinel itself.
                let a = rng.below(sentinel + 1);
                let b = rng.below(sentinel + 1);
                (a.min(b)..a.max(b), ["A", "", "AA", "AAA"][rng.below(4)])
            } else {
                // Edit strictly after the sentinel: start >= sentinel + 1.
                let span = model.len() - (sentinel + 1);
                let a = rng.below(span + 1);
                let b = rng.below(span + 1);
                let base = sentinel + 1;
                (
                    (base + a.min(b))..(base + a.max(b)),
                    ["B", "", "BB"][rng.below(3)],
                )
            };

            let (next, edit) = rope.edit(range.clone(), insert);
            rope = next;
            model.replace_range(range, insert);
            anchor = anchor.rebase(&edit);

            assert_eq!(
                anchor.offset(),
                model.find('\u{1}').unwrap(),
                "anchor drifted from sentinel"
            );
        }
        assert_eq!(rope.slice(anchor.offset()..anchor.offset() + 1), "\u{1}");
    }
}
