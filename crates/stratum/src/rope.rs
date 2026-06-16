// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The copy-on-write [`Rope`] — Majestic's persistent text structure.
//!
//! [`Rope`] is an immutable, height-balanced (AVL) binary tree of UTF-8 chunks. Every
//! internal node caches the [`Summary`] of its subtree, so length queries and coordinate
//! conversions are `O(log n)`. Edits never mutate in place: [`Rope::replace`] splits the
//! tree at the edit boundaries and concatenates the pieces, sharing all untouched nodes
//! with the original through `Arc`. A [`Rope`] is therefore cheap to clone
//! ([`Rope::snapshot`] is an `Arc` bump) and safe to share across threads — the property
//! the editor relies on to run search, highlighting, and agent reads on background threads
//! against a frozen document.
//!
//! # Examples
//! ```
//! use stratum::Rope;
//!
//! let rope = Rope::from("hello\nworld");
//! assert_eq!(rope.len_lines(), 2);
//!
//! let snapshot = rope.snapshot();        // O(1); shares structure
//! let edited = rope.insert(5, ", brave"); // "hello, brave\nworld"
//! assert_eq!(snapshot.to_string(), "hello\nworld"); // original untouched
//! assert_eq!(edited.line(0), "hello, brave");
//! ```

use std::cmp::Ordering;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use crate::summary::{Point, Summary};

/// Maximum bytes held in a single leaf chunk before the builder splits it.
///
/// Chosen as a balance between tree height (smaller chunks → taller tree → more pointer
/// chasing) and copy cost per edit (a leaf is cloned whole when edited, so larger chunks
/// mean more bytes copied per keystroke). 1 KiB keeps both modest; revisit only with a
/// benchmark (Standard §3.2). Adjacent leaves whose combined length fits are coalesced.
const MAX_CHUNK: usize = 1024;

/// A persistent, snapshot-capable UTF-8 text rope.
#[derive(Clone)]
pub struct Rope {
    root: Node,
}

/// A node in the rope: either a text chunk or an internal branch. Cheap to clone.
#[derive(Clone)]
enum Node {
    Leaf(Arc<LeafData>),
    Inner(Arc<InnerData>),
}

struct LeafData {
    text: String,
    summary: Summary,
}

struct InnerData {
    left: Node,
    right: Node,
    summary: Summary,
    height: u32,
}

impl Default for Rope {
    fn default() -> Self {
        Self::new()
    }
}

impl Rope {
    /// Creates an empty rope.
    #[must_use]
    pub fn new() -> Self {
        Self { root: empty_leaf() }
    }

    /// Total number of UTF-8 bytes.
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        node_summary(&self.root).bytes
    }

    /// Total number of Unicode scalar values (`char`s).
    #[must_use]
    pub fn len_chars(&self) -> usize {
        node_summary(&self.root).chars
    }

    /// Number of lines, i.e. one more than the number of newlines.
    ///
    /// An empty rope has one (empty) line, matching editor convention.
    #[must_use]
    pub fn len_lines(&self) -> usize {
        node_summary(&self.root).newlines + 1
    }

    /// Returns `true` if the rope contains no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }

    /// Returns an immutable snapshot of the current contents.
    ///
    /// This is an `O(1)` `Arc` bump: the snapshot shares all nodes with the original and
    /// is unaffected by later edits (which produce new ropes). Snapshots are `Send + Sync`
    /// and meant to be handed to background threads.
    #[must_use]
    pub fn snapshot(&self) -> Self {
        self.clone()
    }

    /// Returns a new rope with `range` (a byte range) replaced by `text`.
    ///
    /// The original is left untouched (copy-on-write). Untouched nodes are shared.
    ///
    /// # Panics
    /// Panics if `range` is out of bounds, inverted, or its endpoints do not fall on
    /// `char` boundaries — all of which are caller contract violations.
    #[must_use]
    pub fn replace(&self, range: Range<usize>, text: &str) -> Self {
        let len = self.len_bytes();
        assert!(
            range.start <= range.end && range.end <= len,
            "replace: range {range:?} out of bounds for rope of {len} bytes"
        );
        self.assert_char_boundary(range.start);
        self.assert_char_boundary(range.end);

        let (left, rest) = split(&self.root, range.start);
        let (_removed, right) = split(&rest, range.end - range.start);
        let middle = node_from_str(text);
        let root = concat(concat(left, middle), right);
        Self { root }
    }

    /// Returns a new rope with `text` inserted at byte offset `at`.
    ///
    /// # Panics
    /// Panics if `at` is out of bounds or not on a `char` boundary.
    #[must_use]
    pub fn insert(&self, at: usize, text: &str) -> Self {
        self.replace(at..at, text)
    }

    /// Returns a new rope with the byte `range` deleted.
    ///
    /// # Panics
    /// Panics if `range` is out of bounds, inverted, or not on `char` boundaries.
    #[must_use]
    pub fn delete(&self, range: Range<usize>) -> Self {
        self.replace(range, "")
    }

    /// Converts a byte offset into a [`Point`] (`row`, byte `column`).
    ///
    /// # Panics
    /// Panics if `byte` exceeds the rope length.
    #[must_use]
    pub fn byte_to_point(&self, byte: usize) -> Point {
        assert!(
            byte <= self.len_bytes(),
            "byte_to_point: offset {byte} past end"
        );
        let row = newlines_before(&self.root, byte);
        let line_start = self.line_start(row);
        Point::new(row, byte - line_start)
    }

    /// Converts a [`Point`] back into a byte offset.
    ///
    /// # Panics
    /// Panics if `point.row` is not a valid line or `point.column` exceeds that line.
    #[must_use]
    pub fn point_to_byte(&self, point: Point) -> usize {
        assert!(
            point.row < self.len_lines(),
            "point_to_byte: row {} past end",
            point.row
        );
        let line_start = self.line_start(point.row);
        let line_end = self.line_end_content(point.row);
        let max_column = line_end - line_start;
        assert!(
            point.column <= max_column,
            "point_to_byte: column {} past end of line {} ({max_column})",
            point.column,
            point.row
        );
        line_start + point.column
    }

    /// Converts a byte offset into a `char` index.
    ///
    /// # Panics
    /// Panics if `byte` exceeds the rope length.
    #[must_use]
    pub fn byte_to_char(&self, byte: usize) -> usize {
        assert!(
            byte <= self.len_bytes(),
            "byte_to_char: offset {byte} past end"
        );
        chars_before(&self.root, byte)
    }

    /// Converts a `char` index into a byte offset.
    ///
    /// # Panics
    /// Panics if `char_idx` exceeds the `char` length.
    #[must_use]
    pub fn char_to_byte(&self, char_idx: usize) -> usize {
        let len_chars = self.len_chars();
        assert!(
            char_idx <= len_chars,
            "char_to_byte: char index {char_idx} past end"
        );
        if char_idx == len_chars {
            return self.len_bytes();
        }
        byte_of_char(&self.root, char_idx)
    }

    /// Returns the text of line `row`, without the trailing newline.
    ///
    /// # Panics
    /// Panics if `row` is not a valid line index.
    #[must_use]
    pub fn line(&self, row: usize) -> String {
        assert!(row < self.len_lines(), "line: row {row} past end");
        let start = self.line_start(row);
        let end = self.line_end_content(row);
        self.slice(start..end)
    }

    /// Returns the bytes in `range` as an owned `String`.
    ///
    /// # Panics
    /// Panics if `range` is out of bounds, inverted, or not on `char` boundaries.
    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> String {
        let len = self.len_bytes();
        assert!(
            range.start <= range.end && range.end <= len,
            "slice: range {range:?} out of bounds for rope of {len} bytes"
        );
        let mut out = String::with_capacity(range.end - range.start);
        let mut pos = 0;
        for chunk in self.chunks() {
            let chunk_end = pos + chunk.len();
            if chunk_end <= range.start {
                pos = chunk_end;
                continue;
            }
            if pos >= range.end {
                break;
            }
            let from = range.start.saturating_sub(pos);
            let to = (range.end - pos).min(chunk.len());
            out.push_str(&chunk[from..to]);
            pos = chunk_end;
        }
        out
    }

    /// Returns an iterator over the rope's text chunks in order.
    ///
    /// Chunk boundaries are an implementation detail (they fall on `char` boundaries but
    /// nowhere predictable); use this for streaming reads such as rendering or search.
    #[must_use]
    pub fn chunks(&self) -> Chunks<'_> {
        Chunks {
            stack: vec![&self.root],
        }
    }

    /// Byte offset where line `row` begins.
    fn line_start(&self, row: usize) -> usize {
        if row == 0 {
            0
        } else {
            newline_offset(&self.root, row - 1) + 1
        }
    }

    /// Byte offset where the content of line `row` ends (before any newline).
    fn line_end_content(&self, row: usize) -> usize {
        if row + 1 < self.len_lines() {
            newline_offset(&self.root, row)
        } else {
            self.len_bytes()
        }
    }

    /// Asserts that `byte` lies on a `char` boundary (or at the end).
    fn assert_char_boundary(&self, byte: usize) {
        assert!(
            is_char_boundary(&self.root, byte),
            "byte offset {byte} is not on a char boundary"
        );
    }
}

impl fmt::Display for Rope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.chunks() {
            f.write_str(chunk)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Rope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rope")
            .field("bytes", &self.len_bytes())
            .field("chars", &self.len_chars())
            .field("lines", &self.len_lines())
            .finish()
    }
}

impl From<&str> for Rope {
    fn from(text: &str) -> Self {
        Self {
            root: node_from_str(text),
        }
    }
}

impl From<String> for Rope {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

/// Iterator over a rope's chunks, yielded left to right.
pub struct Chunks<'a> {
    stack: Vec<&'a Node>,
}

impl fmt::Debug for Chunks<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chunks")
            .field("pending_nodes", &self.stack.len())
            .finish()
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        while let Some(node) = self.stack.pop() {
            match node {
                Node::Leaf(leaf) => {
                    if !leaf.text.is_empty() {
                        return Some(&leaf.text);
                    }
                }
                Node::Inner(inner) => {
                    self.stack.push(&inner.right);
                    self.stack.push(&inner.left);
                }
            }
        }
        None
    }
}

// --- Node construction & accessors -------------------------------------------------

fn empty_leaf() -> Node {
    Node::Leaf(Arc::new(LeafData {
        text: String::new(),
        summary: Summary::ZERO,
    }))
}

fn leaf(text: String) -> Node {
    let summary = Summary::of(&text);
    Node::Leaf(Arc::new(LeafData { text, summary }))
}

fn node_summary(node: &Node) -> Summary {
    match node {
        Node::Leaf(leaf) => leaf.summary,
        Node::Inner(inner) => inner.summary,
    }
}

fn node_height(node: &Node) -> u32 {
    match node {
        Node::Leaf(_) => 1,
        Node::Inner(inner) => inner.height,
    }
}

fn is_empty_leaf(node: &Node) -> bool {
    matches!(node, Node::Leaf(leaf) if leaf.text.is_empty())
}

/// Builds an internal node from two balanced subtrees (no rebalancing).
fn inner_raw(left: Node, right: Node) -> Node {
    let summary = node_summary(&left) + node_summary(&right);
    let height = 1 + node_height(&left).max(node_height(&right));
    Node::Inner(Arc::new(InnerData {
        left,
        right,
        summary,
        height,
    }))
}

// --- Balancing (AVL join) ----------------------------------------------------------

/// Combines two subtrees whose heights differ by at most two, rebalancing if needed.
///
/// Drops empty leaves and coalesces two small leaves. Used only by [`concat`], which
/// guarantees the height-difference precondition.
fn balance(left: Node, right: Node) -> Node {
    if is_empty_leaf(&left) {
        return right;
    }
    if is_empty_leaf(&right) {
        return left;
    }
    if let (Node::Leaf(a), Node::Leaf(b)) = (&left, &right) {
        if a.text.len() + b.text.len() <= MAX_CHUNK {
            let mut text = String::with_capacity(a.text.len() + b.text.len());
            text.push_str(&a.text);
            text.push_str(&b.text);
            return leaf(text);
        }
    }

    let (hl, hr) = (node_height(&left), node_height(&right));
    if hl > hr + 1 {
        // Left heavy by exactly two — rotate right (single or double).
        let Node::Inner(li) = &left else {
            unreachable!("left-heavy node must be internal")
        };
        if node_height(&li.left) >= node_height(&li.right) {
            inner_raw(li.left.clone(), inner_raw(li.right.clone(), right))
        } else {
            let Node::Inner(m) = &li.right else {
                unreachable!("left-right child must be internal")
            };
            inner_raw(
                inner_raw(li.left.clone(), m.left.clone()),
                inner_raw(m.right.clone(), right),
            )
        }
    } else if hr > hl + 1 {
        // Right heavy by exactly two — rotate left (single or double).
        let Node::Inner(ri) = &right else {
            unreachable!("right-heavy node must be internal")
        };
        if node_height(&ri.right) >= node_height(&ri.left) {
            inner_raw(inner_raw(left, ri.left.clone()), ri.right.clone())
        } else {
            let Node::Inner(m) = &ri.left else {
                unreachable!("right-left child must be internal")
            };
            inner_raw(
                inner_raw(left, m.left.clone()),
                inner_raw(m.right.clone(), ri.right.clone()),
            )
        }
    } else {
        inner_raw(left, right)
    }
}

/// Concatenates two ropes, preserving AVL balance, in `O(|height difference|)`.
fn concat(left: Node, right: Node) -> Node {
    if is_empty_leaf(&left) {
        return right;
    }
    if is_empty_leaf(&right) {
        return left;
    }
    let (hl, hr) = (node_height(&left), node_height(&right));
    if hl > hr + 1 {
        let Node::Inner(li) = &left else {
            unreachable!("taller node must be internal")
        };
        balance(li.left.clone(), concat(li.right.clone(), right))
    } else if hr > hl + 1 {
        let Node::Inner(ri) = &right else {
            unreachable!("taller node must be internal")
        };
        balance(concat(left, ri.left.clone()), ri.right.clone())
    } else {
        balance(left, right)
    }
}

/// Splits a subtree at byte offset `at`, returning the pieces before and after it.
///
/// `at` must fall on a `char` boundary (enforced by callers).
fn split(node: &Node, at: usize) -> (Node, Node) {
    match node {
        Node::Leaf(leaf_data) => {
            let (a, b) = leaf_data.text.split_at(at);
            (leaf(a.to_owned()), leaf(b.to_owned()))
        }
        Node::Inner(inner) => {
            let left_bytes = node_summary(&inner.left).bytes;
            match at.cmp(&left_bytes) {
                Ordering::Less => {
                    let (la, lb) = split(&inner.left, at);
                    (la, concat(lb, inner.right.clone()))
                }
                Ordering::Greater => {
                    let (ra, rb) = split(&inner.right, at - left_bytes);
                    (concat(inner.left.clone(), ra), rb)
                }
                Ordering::Equal => (inner.left.clone(), inner.right.clone()),
            }
        }
    }
}

/// Builds a balanced subtree from `text`, chunking it on `char` boundaries.
fn node_from_str(text: &str) -> Node {
    if text.is_empty() {
        return empty_leaf();
    }
    let mut leaves = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    while start < text.len() {
        let mut end = (start + MAX_CHUNK).min(text.len());
        // Back up to the nearest char boundary so we never split a scalar value.
        while end < text.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
            end -= 1;
        }
        leaves.push(leaf(text[start..end].to_owned()));
        start = end;
    }
    build_balanced(&leaves)
}

/// Combines leaves into a balanced tree by recursive midpoint division.
///
/// Splitting at the midpoint keeps the two halves' leaf counts within one of each other,
/// so every node's children differ in height by at most one (the AVL invariant) — unlike
/// naive bottom-up pairing, which strands a tall left subtree against a short remainder.
fn build_balanced(nodes: &[Node]) -> Node {
    match nodes {
        [] => empty_leaf(),
        [only] => only.clone(),
        _ => {
            let mid = nodes.len() / 2;
            inner_raw(build_balanced(&nodes[..mid]), build_balanced(&nodes[mid..]))
        }
    }
}

// --- Metric queries (all O(log n)) -------------------------------------------------

fn newlines_before(node: &Node, byte: usize) -> usize {
    match node {
        Node::Leaf(leaf) => {
            let mut count = 0;
            for &b in &leaf.text.as_bytes()[..byte] {
                if b == b'\n' {
                    count += 1;
                }
            }
            count
        }
        Node::Inner(inner) => {
            let left_bytes = node_summary(&inner.left).bytes;
            if byte <= left_bytes {
                newlines_before(&inner.left, byte)
            } else {
                node_summary(&inner.left).newlines
                    + newlines_before(&inner.right, byte - left_bytes)
            }
        }
    }
}

fn newline_offset(node: &Node, n: usize) -> usize {
    match node {
        Node::Leaf(leaf) => {
            let mut seen = 0;
            for (i, &b) in leaf.text.as_bytes().iter().enumerate() {
                if b == b'\n' {
                    if seen == n {
                        return i;
                    }
                    seen += 1;
                }
            }
            unreachable!("newline_offset: leaf has fewer than {} newlines", n + 1)
        }
        Node::Inner(inner) => {
            let left_newlines = node_summary(&inner.left).newlines;
            if n < left_newlines {
                newline_offset(&inner.left, n)
            } else {
                node_summary(&inner.left).bytes + newline_offset(&inner.right, n - left_newlines)
            }
        }
    }
}

fn chars_before(node: &Node, byte: usize) -> usize {
    match node {
        Node::Leaf(leaf) => leaf.text[..byte].chars().count(),
        Node::Inner(inner) => {
            let left_bytes = node_summary(&inner.left).bytes;
            if byte <= left_bytes {
                chars_before(&inner.left, byte)
            } else {
                node_summary(&inner.left).chars + chars_before(&inner.right, byte - left_bytes)
            }
        }
    }
}

fn byte_of_char(node: &Node, char_idx: usize) -> usize {
    match node {
        Node::Leaf(leaf) => leaf
            .text
            .char_indices()
            .nth(char_idx)
            .map_or(leaf.text.len(), |(i, _)| i),
        Node::Inner(inner) => {
            let left_chars = node_summary(&inner.left).chars;
            if char_idx < left_chars {
                byte_of_char(&inner.left, char_idx)
            } else {
                node_summary(&inner.left).bytes + byte_of_char(&inner.right, char_idx - left_chars)
            }
        }
    }
}

fn is_char_boundary(node: &Node, byte: usize) -> bool {
    match node {
        Node::Leaf(leaf) => byte <= leaf.text.len() && leaf.text.is_char_boundary(byte),
        Node::Inner(inner) => {
            let left_bytes = node_summary(&inner.left).bytes;
            match byte.cmp(&left_bytes) {
                Ordering::Less => is_char_boundary(&inner.left, byte),
                Ordering::Greater => is_char_boundary(&inner.right, byte - left_bytes),
                Ordering::Equal => true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{node_height, node_summary, Node, Rope, MAX_CHUNK};
    use crate::summary::{Point, Summary};

    /// Recursively verifies AVL balance and cached summaries; returns the true summary.
    fn check(node: &Node) -> Summary {
        match node {
            Node::Leaf(leaf) => {
                assert_eq!(leaf.summary, Summary::of(&leaf.text), "leaf summary stale");
                leaf.summary
            }
            Node::Inner(inner) => {
                let l = check(&inner.left);
                let r = check(&inner.right);
                let (hl, hr) = (node_height(&inner.left), node_height(&inner.right));
                let diff = hl.abs_diff(hr);
                assert!(diff <= 1, "AVL imbalance: |{hl} - {hr}| = {diff}");
                assert_eq!(inner.height, 1 + hl.max(hr), "height stale");
                assert_eq!(inner.summary, l + r, "inner summary stale");
                l + r
            }
        }
    }

    fn assert_consistent(rope: &Rope, model: &str) {
        check(&rope.root);
        assert_eq!(rope.to_string(), model, "content diverged");
        assert_eq!(rope.len_bytes(), model.len(), "byte length");
        assert_eq!(rope.len_chars(), model.chars().count(), "char length");
        assert_eq!(
            rope.len_lines(),
            model.bytes().filter(|&b| b == b'\n').count() + 1,
            "line count"
        );
    }

    /// Maps a `char` index in `model` to its byte offset (end maps to `model.len()`).
    fn char_byte(model: &str, char_idx: usize) -> usize {
        model
            .char_indices()
            .nth(char_idx)
            .map_or(model.len(), |(i, _)| i)
    }

    /// Tiny deterministic PRNG (xorshift64*), so failing cases reproduce from the seed.
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

    #[test]
    fn empty_rope_basics() {
        let rope = Rope::new();
        assert!(rope.is_empty());
        assert_eq!(rope.len_bytes(), 0);
        assert_eq!(rope.len_lines(), 1);
        assert_eq!(rope.to_string(), "");
        assert_eq!(rope.byte_to_point(0), Point::new(0, 0));
        assert_eq!(rope.point_to_byte(Point::new(0, 0)), 0);
    }

    #[test]
    fn multibyte_and_lines() {
        let rope = Rope::from("héllo\nwörld😀");
        assert_eq!(rope.len_lines(), 2);
        assert_eq!(rope.len_chars(), "héllo\nwörld😀".chars().count());
        assert_eq!(rope.line(0), "héllo");
        assert_eq!(rope.line(1), "wörld😀");
        // 'w' begins line 1; its byte offset is "héllo\n" in bytes.
        let w = "héllo\n".len();
        assert_eq!(rope.byte_to_point(w), Point::new(1, 0));
        assert_eq!(rope.point_to_byte(Point::new(1, 0)), w);
        // Round-trip byte <-> char on a multibyte boundary.
        let smiley = "héllo\nwörld".len();
        assert_eq!(rope.char_to_byte(rope.byte_to_char(smiley)), smiley);
    }

    #[test]
    fn snapshot_is_immutable() {
        let original = Rope::from("hello world");
        let snap = original.snapshot();
        let edited = original.insert(5, ",");
        assert_eq!(snap.to_string(), "hello world");
        assert_eq!(edited.to_string(), "hello, world");
    }

    #[test]
    fn rope_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Rope>();
    }

    #[test]
    fn large_build_chunks_and_balances() {
        let text: String = "abcdefghijklmnopqrstuvwxyz"
            .chars()
            .cycle()
            .take(5000)
            .collect();
        let rope = Rope::from(text.as_str());
        assert_consistent(&rope, &text);
        assert!(rope.len_bytes() > MAX_CHUNK, "should span many chunks");
        assert_eq!(rope.slice(100..200), text[100..200]);
    }

    #[test]
    fn model_based_random_edits() {
        for seed in [1_u64, 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0, 42] {
            let mut rng = Rng(seed);
            let mut rope = Rope::new();
            let mut model = String::new();
            let inserts = ["a", "X", "\n", "héllo", "😀", "lorem ipsum ", "\n\n", "—"];

            for _ in 0..3000 {
                if model.is_empty() || rng.below(3) != 0 {
                    // Insert at a random char boundary.
                    let char_count = model.chars().count();
                    let at_char = rng.below(char_count + 1);
                    let at = char_byte(&model, at_char);
                    let text = inserts[rng.below(inserts.len())];
                    rope = rope.insert(at, text);
                    model.insert_str(at, text);
                } else {
                    // Delete a random char-bounded range.
                    let char_count = model.chars().count();
                    let a = rng.below(char_count + 1);
                    let b = rng.below(char_count + 1);
                    let (lo, hi) = (a.min(b), a.max(b));
                    let (start, end) = (char_byte(&model, lo), char_byte(&model, hi));
                    rope = rope.delete(start..end);
                    model.replace_range(start..end, "");
                }
                assert_consistent(&rope, &model);
            }

            // Coordinate conversions agree with the model at every line start.
            for row in 0..rope.len_lines() {
                let byte = rope.point_to_byte(Point::new(row, 0));
                assert_eq!(rope.byte_to_point(byte), Point::new(row, 0));
            }
            // Summary cached at the root matches a fresh scan.
            assert_eq!(node_summary(&rope.root), Summary::of(&model));
        }
    }
}
