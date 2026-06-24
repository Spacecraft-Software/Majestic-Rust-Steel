// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Tagged buffer reads and edits — the agent-facing layer over [`stratum`]'s hashline tags
//! (PRD #1 §5.2.5).
//!
//! [`tagged_read`] renders a buffer as `LINE:TAG│text` lines (1-based `LINE`); the agent then cites
//! those `LINE:TAG` anchors in [`HashlineEdit`]s instead of re-emitting old text. [`apply`] is the
//! pre-approval gate: it recomputes every cited tag against the live buffer and rejects the **whole**
//! batch if any is stale (the line changed since the agent read it), then applies the fresh edits
//! back-to-front so earlier byte offsets stay valid. Resolving and rejecting before anything mutates
//! is what closes the read→propose TOCTOU gap; in M3 this is invoked behind Seraph.

use std::fmt;

use stratum::{tag_matches, LineTags, Point};

use crate::buffer::Buffer;

/// The separator between an agent-read line's `LINE:TAG` prefix and its text (`│`, U+2502). A box
/// glyph the agent will not confuse with code, and which never appears in a tag.
const READ_SEPARATOR: char = '│';

/// A reference to a buffer line by its 0-based number and its hashline tag — the `LINE:TAG` an agent
/// cites. (The agent sees 1-based numbers in [`tagged_read`]; the tool layer converts.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineRef {
    /// The 0-based line number.
    pub line: usize,
    /// The hashline tag the agent read for that line.
    pub tag: String,
}

impl LineRef {
    /// Creates a reference to 0-based `line` with `tag`.
    #[must_use]
    pub fn new(line: usize, tag: impl Into<String>) -> Self {
        Self {
            line,
            tag: tag.into(),
        }
    }
}

/// One tagged edit an agent proposes against a buffer. Each cites the line it targets by `LINE:TAG`,
/// so the edit only applies while that tag is still fresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HashlineEdit {
    /// Replace the cited line's content with `text` (its line ending is preserved).
    Replace {
        /// The line whose content is replaced.
        at: LineRef,
        /// The replacement content (no trailing newline).
        text: String,
    },
    /// Insert `text` as a new line immediately after the cited line.
    InsertAfter {
        /// The line the new line is inserted after.
        at: LineRef,
        /// The content of the inserted line (no trailing newline).
        text: String,
    },
    /// Delete the cited line (and its line ending).
    Delete {
        /// The line to delete.
        at: LineRef,
    },
}

impl HashlineEdit {
    /// The line this edit cites.
    fn anchor(&self) -> &LineRef {
        match self {
            Self::Replace { at, .. } | Self::InsertAfter { at, .. } | Self::Delete { at } => at,
        }
    }
}

/// Why a tagged edit could not be applied. Both reject the whole batch before any mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HashlineError {
    /// The cited line is past the end of the buffer.
    OutOfRange {
        /// The 0-based line the edit cited.
        line: usize,
        /// How many lines the buffer has.
        lines: usize,
    },
    /// The cited tag no longer matches the live line — the buffer changed since the agent read it, so
    /// the agent must re-read. This is the stale-tag rejection (the TOCTOU gate).
    StaleTag {
        /// The 0-based line the edit cited.
        line: usize,
        /// The tag the agent cited.
        cited: String,
        /// The tag the live line has now.
        actual: String,
    },
}

impl fmt::Display for HashlineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { line, lines } => write!(
                f,
                "line {} is out of range (buffer has {lines} lines)",
                line + 1
            ),
            Self::StaleTag {
                line,
                cited,
                actual,
            } => write!(
                f,
                "line {} changed since it was read (tag {cited} is now {actual}); re-read before editing",
                line + 1
            ),
        }
    }
}

impl std::error::Error for HashlineError {}

/// Renders the whole `buffer` as tagged lines, one per line: `LINE:TAG│text`, with `LINE` 1-based.
/// This is what an agent reads before proposing [`HashlineEdit`]s. Tags come from
/// [`stratum::LineTags`], so distinct lines get distinct tags (byte-identical lines share one and are
/// told apart by `LINE`).
#[must_use]
pub fn tagged_read(buffer: &Buffer) -> String {
    let rope = buffer.rope();
    let contents: Vec<String> = (0..rope.len_lines()).map(|row| rope.line(row)).collect();
    let line_bytes: Vec<&[u8]> = contents.iter().map(String::as_bytes).collect();
    let tags = LineTags::compute(&line_bytes);

    let mut out = String::new();
    for (row, content) in contents.iter().enumerate() {
        let tag = tags.tag(row).unwrap_or("");
        // `LINE:TAG│text\n` with a 1-based `LINE`, built without an intermediate format allocation.
        out.push_str(&(row + 1).to_string());
        out.push(':');
        out.push_str(tag);
        out.push(READ_SEPARATOR);
        out.push_str(content);
        out.push('\n');
    }
    out
}

/// Applies `edits` to `buffer` atomically. First every cited tag is checked against the live buffer;
/// a single stale (or out-of-range) reference rejects the **entire** batch with no mutation — the
/// pre-approval gate. The accepted edits are then applied back-to-front (highest line first) so the
/// byte offsets of the earlier ones stay valid.
///
/// # Errors
/// Returns [`HashlineError::OutOfRange`] if an edit cites a line past the buffer's end, or
/// [`HashlineError::StaleTag`] if a cited tag no longer matches the live line.
pub fn apply(buffer: &mut Buffer, edits: &[HashlineEdit]) -> Result<(), HashlineError> {
    let rope = buffer.rope();
    let line_count = rope.len_lines();

    // Gate: verify every cited tag against the live buffer before touching anything.
    for edit in edits {
        let anchor = edit.anchor();
        if anchor.line >= line_count {
            return Err(HashlineError::OutOfRange {
                line: anchor.line,
                lines: line_count,
            });
        }
        let live = rope.line(anchor.line);
        if !tag_matches(live.as_bytes(), &anchor.tag) {
            return Err(HashlineError::StaleTag {
                line: anchor.line,
                cited: anchor.tag.clone(),
                actual: stratum::line_tag(live.as_bytes(), anchor.tag.chars().count()),
            });
        }
    }

    // Resolve each edit to a (byte range, replacement) against the original snapshot, then apply
    // highest-line first so the lower-line ranges remain valid as the buffer shrinks/grows above them.
    let mut resolved: Vec<(std::ops::Range<usize>, String)> = edits
        .iter()
        .map(|edit| resolve(&rope, line_count, edit))
        .collect();
    resolved.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    for (range, replacement) in resolved {
        buffer.replace_range(range, &replacement);
    }
    Ok(())
}

/// Turns one validated edit into the byte range to splice and the text to put there, against `rope`.
fn resolve(
    rope: &stratum::Rope,
    line_count: usize,
    edit: &HashlineEdit,
) -> (std::ops::Range<usize>, String) {
    let line = edit.anchor().line;
    let start = rope.point_to_byte(Point::new(line, 0));
    let content_end = start + rope.line(line).len();
    let is_last = line + 1 >= line_count;
    match edit {
        HashlineEdit::Replace { text, .. } => (start..content_end, text.clone()),
        HashlineEdit::InsertAfter { text, .. } => {
            if is_last {
                // No trailing newline on the last line: prepend one so `text` becomes the next line.
                (rope.len_bytes()..rope.len_bytes(), format!("\n{text}"))
            } else {
                let next = rope.point_to_byte(Point::new(line + 1, 0));
                (next..next, format!("{text}\n"))
            }
        }
        HashlineEdit::Delete { .. } => {
            if line_count == 1 {
                (0..rope.len_bytes(), String::new()) // the only line: clear it
            } else if is_last {
                (start - 1..rope.len_bytes(), String::new()) // eat the preceding newline
            } else {
                let next = rope.point_to_byte(Point::new(line + 1, 0));
                (start..next, String::new()) // eat the content and its trailing newline
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{apply, tagged_read, HashlineEdit, HashlineError, LineRef};
    use crate::buffer::Buffer;
    use stratum::line_tag;

    /// The tag `tagged_read` assigned to 0-based `line`.
    fn tag_of(buffer: &Buffer, line: usize) -> String {
        let read = tagged_read(buffer);
        let row = read.lines().nth(line).expect("line present");
        // `LINE:TAG│text` — take the TAG between the first `:` and the `│`.
        let after_colon = row.split_once(':').expect("has colon").1;
        after_colon
            .split_once('│')
            .expect("has separator")
            .0
            .to_owned()
    }

    #[test]
    fn tagged_read_is_one_to_one_with_lines() {
        let buffer = Buffer::from_text("alpha\nbeta\ngamma");
        let read = tagged_read(&buffer);
        let lines: Vec<&str> = read.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("1:") && lines[0].ends_with("│alpha"));
        assert!(lines[2].starts_with("3:") && lines[2].ends_with("│gamma"));
    }

    #[test]
    fn replace_with_a_fresh_tag_applies() {
        let mut buffer = Buffer::from_text("one\ntwo\nthree");
        let tag = tag_of(&buffer, 1);
        let edit = HashlineEdit::Replace {
            at: LineRef::new(1, tag),
            text: "TWO".to_owned(),
        };
        apply(&mut buffer, &[edit]).expect("fresh tag applies");
        assert_eq!(buffer.text(), "one\nTWO\nthree");
    }

    #[test]
    fn a_stale_tag_rejects_the_batch_without_mutating() {
        let mut buffer = Buffer::from_text("one\ntwo\nthree");
        // A tag for content that is not actually on line 1.
        let stale = line_tag(b"something else", 2);
        let edits = vec![
            HashlineEdit::Replace {
                at: LineRef::new(0, tag_of(&buffer, 0)),
                text: "ONE".to_owned(),
            },
            HashlineEdit::Replace {
                at: LineRef::new(1, stale),
                text: "TWO".to_owned(),
            },
        ];
        let error = apply(&mut buffer, &edits).expect_err("stale tag rejects");
        assert!(matches!(error, HashlineError::StaleTag { line: 1, .. }));
        assert_eq!(
            buffer.text(),
            "one\ntwo\nthree",
            "nothing mutates on rejection"
        );
    }

    #[test]
    fn insert_after_and_delete_resolve_correctly() {
        let mut buffer = Buffer::from_text("a\nb\nc");
        let insert = HashlineEdit::InsertAfter {
            at: LineRef::new(0, tag_of(&buffer, 0)),
            text: "NEW".to_owned(),
        };
        apply(&mut buffer, &[insert]).expect("insert applies");
        assert_eq!(buffer.text(), "a\nNEW\nb\nc");

        // Delete the last line (eats the preceding newline).
        let delete = HashlineEdit::Delete {
            at: LineRef::new(3, tag_of(&buffer, 3)),
        };
        apply(&mut buffer, &[delete]).expect("delete applies");
        assert_eq!(buffer.text(), "a\nNEW\nb");
    }

    #[test]
    fn a_batch_applies_back_to_front() {
        // Editing two lines at once must not invalidate the lower line's offsets.
        let mut buffer = Buffer::from_text("one\ntwo\nthree\nfour");
        let edits = vec![
            HashlineEdit::Replace {
                at: LineRef::new(0, tag_of(&buffer, 0)),
                text: "1".to_owned(),
            },
            HashlineEdit::Delete {
                at: LineRef::new(2, tag_of(&buffer, 2)),
            },
        ];
        apply(&mut buffer, &edits).expect("batch applies");
        assert_eq!(buffer.text(), "1\ntwo\nfour");
    }

    #[test]
    fn out_of_range_line_is_rejected() {
        let mut buffer = Buffer::from_text("only");
        let edit = HashlineEdit::Delete {
            at: LineRef::new(5, "xx"),
        };
        let error = apply(&mut buffer, &[edit]).expect_err("out of range");
        assert!(matches!(
            error,
            HashlineError::OutOfRange { line: 5, lines: 1 }
        ));
    }
}
