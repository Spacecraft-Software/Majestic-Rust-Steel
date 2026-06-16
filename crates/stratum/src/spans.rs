// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Interval-keyed metadata [`Span`]s and [`SpanLayer`]s that rebase through edits.
//!
//! A [`Span`] attaches a value to a byte interval whose endpoints are [`Anchor`]s, so it
//! moves with the text as it is edited. Spans are grouped into layers — one [`SpanLayer`]
//! per concern: syntax captures, diagnostics, selections, and the regions Seraph marks
//! pending. A layer is kept sorted by start offset, supports overlap queries for rendering
//! a viewport, and is rebased as a whole through an [`Edit`].
//!
//! Endpoints carry a [`Bias`], so a span can decide whether text inserted exactly at its
//! edges is absorbed. [`Span::with_offsets`] builds the safe default — an *edge-exclusive*
//! span (start leans [`Right`](Bias::Right), end leans [`Left`](Bias::Left)) — so typing at
//! a boundary shifts the span rather than silently extending it.
//!
//! # Examples
//! ```
//! use stratum::{Rope, Span, SpanLayer};
//!
//! let rope = Rope::from("let x = 1;");
//! let mut layer = SpanLayer::new();
//! layer.insert(Span::with_offsets(0, 3, "keyword")); // "let"
//!
//! let (_rope, edit) = rope.edit(0..0, "    "); // indent the line
//! let layer = layer.rebase(&edit);
//! assert_eq!(layer.iter().next().unwrap().range(), 4..7);
//! ```

use std::ops::Range;

use crate::anchor::{Anchor, Bias, Edit};

/// A value attached to a byte interval whose endpoints survive edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span<T> {
    /// Inclusive start position.
    pub start: Anchor,
    /// Exclusive end position.
    pub end: Anchor,
    /// The associated metadata.
    pub value: T,
}

impl<T> Span<T> {
    /// Creates a span from explicit start and end anchors.
    #[must_use]
    pub fn new(start: Anchor, end: Anchor, value: T) -> Self {
        Self { start, end, value }
    }

    /// Creates an edge-exclusive span over `start..end` (start leans right, end leans left).
    ///
    /// Text inserted exactly at either boundary stays outside the span; the span shifts to
    /// follow its content. This is the safe default for syntax and diagnostic spans.
    #[must_use]
    pub fn with_offsets(start: usize, end: usize, value: T) -> Self {
        Self {
            start: Anchor::new(start, Bias::Right),
            end: Anchor::new(end, Bias::Left),
            value,
        }
    }

    /// The span's current byte range.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.start.offset()..self.end.offset()
    }

    /// Returns the span with both endpoints rebased through `edit`.
    #[must_use]
    pub fn rebase(&self, edit: &Edit) -> Self
    where
        T: Clone,
    {
        Self {
            start: self.start.rebase(edit),
            end: self.end.rebase(edit),
            value: self.value.clone(),
        }
    }
}

/// An ordered collection of [`Span`]s for a single concern (a metadata layer).
#[derive(Clone, Debug)]
pub struct SpanLayer<T> {
    spans: Vec<Span<T>>,
}

impl<T> Default for SpanLayer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> SpanLayer<T> {
    /// Creates an empty layer.
    #[must_use]
    pub const fn new() -> Self {
        Self { spans: Vec::new() }
    }

    /// Number of spans in the layer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Returns `true` if the layer has no spans.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Inserts a span, keeping the layer sorted by start offset.
    pub fn insert(&mut self, span: Span<T>) {
        let at = self
            .spans
            .partition_point(|existing| existing.start.offset() <= span.start.offset());
        self.spans.insert(at, span);
    }

    /// Iterates the spans in start-offset order.
    pub fn iter(&self) -> impl Iterator<Item = &Span<T>> {
        self.spans.iter()
    }

    /// Iterates the spans that overlap the half-open byte `range`.
    pub fn spans_in(&self, range: Range<usize>) -> impl Iterator<Item = &Span<T>> {
        self.spans.iter().filter(move |span| {
            let r = span.range();
            r.end > range.start && r.start < range.end
        })
    }
}

impl<T: Clone> SpanLayer<T> {
    /// Returns the layer with every span rebased through `edit`, re-sorted by start offset.
    #[must_use]
    pub fn rebase(&self, edit: &Edit) -> Self {
        let mut spans: Vec<Span<T>> = self.spans.iter().map(|span| span.rebase(edit)).collect();
        spans.sort_by_key(|span| span.start.offset());
        Self { spans }
    }
}

#[cfg(test)]
mod tests {
    use super::{Span, SpanLayer};
    use crate::Rope;

    #[test]
    fn rebase_shifts_inserts_and_collapses_deletes() {
        let mut layer = SpanLayer::new();
        layer.insert(Span::with_offsets(3, 6, "x"));

        // Insert before the span -> it shifts right by the inserted length.
        let shifted = layer.rebase(&Rope::from("0123456789").edit(0..0, "ab").1);
        assert_eq!(shifted.iter().next().unwrap().range(), 5..8);

        // Delete that covers the span -> it collapses to a zero-length point.
        let collapsed = shifted.rebase(&Rope::from("ab0123456789").edit(5..8, "").1);
        assert_eq!(collapsed.iter().next().unwrap().range(), 5..5);
    }

    #[test]
    fn edge_exclusive_does_not_absorb_boundary_inserts() {
        let mut layer = SpanLayer::new();
        layer.insert(Span::with_offsets(3, 6, "x"));
        // Insert exactly at the end boundary: span must not grow.
        let edited = layer.rebase(&Rope::from("0123456789").edit(6..6, "ZZ").1);
        assert_eq!(edited.iter().next().unwrap().range(), 3..6);
    }

    #[test]
    fn insert_keeps_layer_sorted() {
        let mut layer = SpanLayer::new();
        layer.insert(Span::with_offsets(10, 12, "c"));
        layer.insert(Span::with_offsets(0, 3, "a"));
        layer.insert(Span::with_offsets(5, 8, "b"));
        let starts: Vec<usize> = layer.iter().map(|s| s.start.offset()).collect();
        assert_eq!(starts, [0, 5, 10]);
    }

    #[test]
    fn spans_in_returns_overlapping_only() {
        let mut layer = SpanLayer::new();
        layer.insert(Span::with_offsets(0, 3, "a"));
        layer.insert(Span::with_offsets(5, 8, "b"));
        layer.insert(Span::with_offsets(10, 12, "c"));
        let hit: Vec<&str> = layer.spans_in(4..6).map(|s| s.value).collect();
        assert_eq!(hit, ["b"]);
        let hit: Vec<&str> = layer.spans_in(2..11).map(|s| s.value).collect();
        assert_eq!(hit, ["a", "b", "c"]);
    }

    /// Tiny deterministic PRNG (xorshift64*), mirroring the other stratum test harnesses.
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

    /// An edge-exclusive span over a run of 'M's must keep covering exactly that run
    /// through random edits in the surrounding regions that never touch the 'M's.
    #[test]
    fn span_tracks_middle_run_through_random_edits() {
        let mut rng = Rng(0x5A11_5EED);
        let mut model = String::from("AAAMMMMMBBB");
        let mut rope = Rope::from(model.as_str());
        let m_start = model.find('M').unwrap();
        let mut layer = SpanLayer::new();
        layer.insert(Span::with_offsets(m_start, m_start + 5, "M"));

        for _ in 0..2000 {
            let m_start = model.find('M').unwrap();
            let m_end = m_start + 5;
            let (range, insert) = if rng.below(2) == 0 {
                // Prefix region [0, m_start]; never deletes into the 'M's.
                let a = rng.below(m_start + 1);
                let b = rng.below(m_start + 1);
                (a.min(b)..a.max(b), ["A", "", "AA"][rng.below(3)])
            } else {
                // Suffix region [m_end, len]; never deletes into the 'M's.
                let span = model.len() - m_end;
                let a = rng.below(span + 1);
                let b = rng.below(span + 1);
                (
                    (m_end + a.min(b))..(m_end + a.max(b)),
                    ["B", "", "BB"][rng.below(3)],
                )
            };

            let (next, edit) = rope.edit(range.clone(), insert);
            rope = next;
            model.replace_range(range, insert);
            layer = layer.rebase(&edit);

            let got = layer.iter().next().unwrap().range();
            assert_eq!(rope.slice(got), "MMMMM", "span drifted off the M run");
        }
    }
}
