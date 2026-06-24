// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Hashline — BLAKE3 line tags for governed agent edits (PRD #1 §5.2.5).
//!
//! When an AI agent reads a buffer it sees every line as `LINE:TAG│text`, where `TAG` is a short
//! base32 hash of the line's content. The agent then edits *by reference* (`replace 2:f1 …`) instead
//! of re-emitting whitespace-perfect old text. The tag is a freshness proof: before an edit applies,
//! the host recomputes the live line's tag and compares — a [`tag_matches`] mismatch means the buffer
//! changed since the agent read it (a *stale tag*), so the edit is rejected and the agent must
//! re-read. This closes the read→propose time-of-check/time-of-use gap by construction (the rejection
//! is Seraph's pre-approval gate in M3).
//!
//! This module is the pure tag engine: it hashes line bytes and resolves collisions. Computing tags
//! from the rope's chunk summaries (amortizing the hash) and the tagged read/edit tools layer on top.

use std::collections::HashMap;

/// The default tag width in base32 characters — 10 bits of the BLAKE3 digest. Short enough to be
/// cheap in an agent's token budget, wide enough that collisions between distinct lines are rare;
/// a colliding group is widened (see [`LineTags::compute`]) until its lines separate.
pub const DEFAULT_TAG_WIDTH: usize = 2;

/// The widest a tag grows. Seven base32 chars is 35 bits of digest; byte-identical lines never
/// separate however wide the tag (they share a tag and are told apart by line number), so widening
/// past this cannot help and only spends the agent's tokens.
pub const MAX_TAG_WIDTH: usize = 7;

/// The base32 alphabet (RFC 4648, lowercased) — five bits per character. Lowercase keeps tags visually
/// quiet in the agent's read and avoids case-folding ambiguity.
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The hashline tag of `width` base32 characters for one line, **ending-agnostic**: a trailing `\n`
/// then `\r` is stripped before hashing, so the tag is over the line's content (LF and CRLF forms of
/// the same line tag alike). The same content always yields the same tag; any content change yields a
/// different one — the freshness guarantee. `width` is clamped to `1..=`[`MAX_TAG_WIDTH`].
#[must_use]
pub fn line_tag(line: &[u8], width: usize) -> String {
    let digest = blake3::hash(strip_line_ending(line));
    base32_prefix(digest.as_bytes(), width.clamp(1, MAX_TAG_WIDTH))
}

/// Whether `tag` still matches `line`'s content — the stale-tag check, evaluated at the tag's own
/// width. A reference whose tag no longer matches the live line is stale and must be rejected so the
/// agent re-reads.
#[must_use]
pub fn tag_matches(line: &[u8], tag: &str) -> bool {
    line_tag(line, tag.chars().count()) == tag
}

/// Strips a single trailing `\n` and then a single trailing `\r`, so the tag covers a line's content
/// independent of its line ending.
fn strip_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Encodes the first `width` base32 characters (5 bits each) of `digest`, most-significant bits first.
fn base32_prefix(digest: &[u8], width: usize) -> String {
    let mut out = String::with_capacity(width);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    let mut bytes = digest.iter();
    for _ in 0..width {
        while bits < 5 {
            // BLAKE3 is 32 bytes; `width <= MAX_TAG_WIDTH` needs at most 5 of them, so this never
            // runs dry, but default to 0 defensively rather than panic.
            accumulator = (accumulator << 8) | u32::from(bytes.next().copied().unwrap_or(0));
            bits += 8;
        }
        bits -= 5;
        let index = ((accumulator >> bits) & 0x1f) as usize;
        out.push(char::from(BASE32[index]));
    }
    out
}

/// Every line's hashline tag for one document snapshot, widened per colliding group so that distinct
/// lines get distinct tags. Byte-identical lines necessarily share a tag — no hash can separate equal
/// content — and are told apart by their line number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineTags {
    tags: Vec<String>,
}

impl LineTags {
    /// Computes the tag for every line in `lines` (each entry one line's bytes): starts every tag at
    /// [`DEFAULT_TAG_WIDTH`] and repeatedly widens any group of lines sharing a tag whose members are
    /// not all byte-identical, until each such group separates or reaches [`MAX_TAG_WIDTH`].
    #[must_use]
    pub fn compute(lines: &[&[u8]]) -> Self {
        let mut tags: Vec<String> = lines
            .iter()
            .map(|line| line_tag(line, DEFAULT_TAG_WIDTH))
            .collect();
        loop {
            // Group line indices by their current tag, then choose which to widen. The borrow of
            // `tags` (via the `&str` keys) is confined to this block so the widening below can mutate.
            let widen: Vec<usize> = {
                let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
                for (index, tag) in tags.iter().enumerate() {
                    groups.entry(tag.as_str()).or_default().push(index);
                }
                let mut widen = Vec::new();
                for indices in groups.values() {
                    if indices.len() < 2 || tags[indices[0]].len() >= MAX_TAG_WIDTH {
                        continue;
                    }
                    let first = strip_line_ending(lines[indices[0]]);
                    if indices
                        .iter()
                        .all(|&i| strip_line_ending(lines[i]) == first)
                    {
                        continue; // byte-identical lines cannot be separated by a wider hash
                    }
                    widen.extend_from_slice(indices);
                }
                widen
            };
            if widen.is_empty() {
                break;
            }
            for index in widen {
                tags[index] = line_tag(lines[index], tags[index].len() + 1);
            }
        }
        Self { tags }
    }

    /// The tag for 0-based `line`, or `None` when out of range.
    #[must_use]
    pub fn tag(&self, line: usize) -> Option<&str> {
        self.tags.get(line).map(String::as_str)
    }

    /// The number of lines tagged.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tags.len()
    }

    /// Whether no lines are tagged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{line_tag, tag_matches, LineTags, DEFAULT_TAG_WIDTH, MAX_TAG_WIDTH};

    #[test]
    fn tag_is_deterministic_and_the_requested_width() {
        assert_eq!(line_tag(b"let x = 1;", 2), line_tag(b"let x = 1;", 2));
        assert_eq!(line_tag(b"let x = 1;", 2).chars().count(), 2);
        assert_eq!(line_tag(b"let x = 1;", 5).chars().count(), 5);
        // Only the base32 alphabet appears.
        assert!(line_tag(b"anything", 7)
            .chars()
            .all(|c| "abcdefghijklmnopqrstuvwxyz234567".contains(c)));
    }

    #[test]
    fn tag_is_ending_agnostic_but_content_sensitive() {
        // LF, CRLF, and bare content of the same line tag alike.
        assert_eq!(line_tag(b"foo", 4), line_tag(b"foo\n", 4));
        assert_eq!(line_tag(b"foo", 4), line_tag(b"foo\r\n", 4));
        // Any content change (here a trailing space) changes the tag.
        assert_ne!(line_tag(b"foo", 4), line_tag(b"foo ", 4));
    }

    #[test]
    fn tag_matches_detects_change() {
        let tag = line_tag(b"original line", DEFAULT_TAG_WIDTH);
        assert!(tag_matches(b"original line", &tag)); // unchanged → fresh
        assert!(tag_matches(b"original line\n", &tag)); // ending-agnostic
        assert!(!tag_matches(b"edited line", &tag)); // changed → stale
    }

    #[test]
    fn widening_separates_distinct_lines_at_scale() {
        // 300 distinct lines: with 2-char (1024-value) tags, collisions are near-certain, so the
        // widener must kick in. Every distinct line must end with a distinct tag.
        let owned: Vec<String> = (0..300).map(|i| format!("line number {i}")).collect();
        let lines: Vec<&[u8]> = owned.iter().map(String::as_bytes).collect();
        let tags = LineTags::compute(&lines);
        assert_eq!(tags.len(), 300);

        let mut seen = std::collections::HashSet::new();
        for line in 0..tags.len() {
            let tag = tags.tag(line).expect("every line has a tag");
            assert!(tag.len() <= MAX_TAG_WIDTH);
            assert!(
                seen.insert(tag.to_owned()),
                "distinct lines must get distinct tags: {tag}"
            );
        }
    }

    #[test]
    fn byte_identical_lines_share_a_tag() {
        // Two blank lines and two `}` lines: each pair is byte-identical and cannot be separated, so
        // they share a tag (the line number tells them apart); the distinct line keeps its own tag.
        let lines: Vec<&[u8]> = vec![b"", b"}", b"", b"}", b"distinct"];
        let tags = LineTags::compute(&lines);
        assert_eq!(tags.tag(0), tags.tag(2)); // the two blank lines
        assert_eq!(tags.tag(1), tags.tag(3)); // the two `}` lines
        assert_ne!(tags.tag(0), tags.tag(1));
        assert_ne!(tags.tag(4), tags.tag(0));
    }

    #[test]
    fn out_of_range_line_has_no_tag() {
        let lines: Vec<&[u8]> = vec![b"only line"];
        let tags = LineTags::compute(&lines);
        assert!(tags.tag(0).is_some());
        assert_eq!(tags.tag(1), None);
        assert!(!tags.is_empty());
    }
}
