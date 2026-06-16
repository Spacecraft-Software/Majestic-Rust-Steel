// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The editable [`Buffer`] — a document model over Stratum's rope and undo tree.
//!
//! A [`Buffer`] owns a [`stratum::UndoTree`] (whose current node is the live rope), a byte
//! cursor kept on a `char` boundary, an optional selection anchor, and a goal column for
//! vertical motion. Every edit goes through one path that records a new undo node and moves
//! the cursor, so undo/redo and editing stay consistent. Crash-safe journaling and recovery
//! are wired in with the editor loop (M0 step 7); this layer is pure in-memory + file I/O.

use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use stratum::{Point, Rope, UndoTree};

/// An editable text document: content, cursor, selection, and undo history.
#[derive(Debug)]
pub struct Buffer {
    history: UndoTree,
    cursor: usize,
    selection_anchor: Option<usize>,
    goal_column: Option<usize>,
    path: Option<PathBuf>,
    dirty: bool,
}

impl Buffer {
    /// Creates an empty, unnamed buffer.
    #[must_use]
    pub fn scratch() -> Self {
        Self::from_rope(Rope::new(), None)
    }

    /// Creates an unnamed buffer holding `text`.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::from_rope(Rope::from(text), None)
    }

    fn from_rope(rope: Rope, path: Option<PathBuf>) -> Self {
        Self {
            history: UndoTree::new(rope),
            cursor: 0,
            selection_anchor: None,
            goal_column: None,
            path,
            dirty: false,
        }
    }

    /// Opens `path`, reading its contents (an empty buffer if the file does not exist).
    ///
    /// # Errors
    /// Returns an I/O error if the file exists but cannot be read.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        Ok(Self::from_rope(Rope::from(text.as_str()), Some(path)))
    }

    /// Writes the buffer to its path and clears the dirty flag.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] if the buffer has no path, or any write error.
    pub fn save(&mut self) -> io::Result<()> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer has no path"))?;
        fs::write(path, self.text())?;
        self.dirty = false;
        Ok(())
    }

    // --- Queries ---------------------------------------------------------------------

    /// The live rope.
    #[must_use]
    pub fn rope(&self) -> &Rope {
        self.history.current_rope()
    }

    /// The buffer contents as a `String`.
    #[must_use]
    pub fn text(&self) -> String {
        self.rope().to_string()
    }

    /// The cursor's byte offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor's `(row, byte-column)` position.
    #[must_use]
    pub fn cursor_point(&self) -> Point {
        self.rope().byte_to_point(self.cursor)
    }

    /// The cursor's column counted in `char`s from the start of its line (the display column).
    #[must_use]
    pub fn cursor_column(&self) -> usize {
        let rope = self.rope();
        let line_start = self.line_start(rope.byte_to_point(self.cursor).row);
        rope.byte_to_char(self.cursor) - rope.byte_to_char(line_start)
    }

    /// Whether the buffer has unsaved changes.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The buffer's file path, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The selected byte range, if a (non-empty) selection is active.
    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        let (lo, hi) = (anchor.min(self.cursor), anchor.max(self.cursor));
        (lo != hi).then_some(lo..hi)
    }

    /// The selected text, if any.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|range| self.rope().slice(range))
    }

    // --- Editing ---------------------------------------------------------------------

    /// Inserts `text` at the cursor, replacing the selection if one is active.
    pub fn insert(&mut self, text: &str) {
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.apply_edit(range, text);
    }

    /// Inserts a single character (convenience for self-insert).
    pub fn insert_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        self.insert(ch.encode_utf8(&mut buf));
    }

    /// Deletes the selection, or the character before the cursor (Backspace).
    pub fn backspace(&mut self) {
        if let Some(range) = self.selection() {
            self.apply_edit(range, "");
        } else {
            let prev = self.prev_boundary(self.cursor);
            if prev < self.cursor {
                self.apply_edit(prev..self.cursor, "");
            }
        }
    }

    /// Deletes the selection, or the character at the cursor (Delete).
    pub fn delete_forward(&mut self) {
        if let Some(range) = self.selection() {
            self.apply_edit(range, "");
        } else {
            let next = self.next_boundary(self.cursor);
            if next > self.cursor {
                self.apply_edit(self.cursor..next, "");
            }
        }
    }

    /// Deletes the active selection if any; returns whether anything was removed.
    pub fn delete_selection(&mut self) -> bool {
        if let Some(range) = self.selection() {
            self.apply_edit(range, "");
            true
        } else {
            false
        }
    }

    /// Selects the entire buffer.
    pub fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.cursor = self.rope().len_bytes();
        self.goal_column = None;
    }

    /// Undoes the last edit, if any; returns whether the buffer changed.
    pub fn undo(&mut self) -> bool {
        if self.history.undo().is_some() {
            self.after_history_move();
            true
        } else {
            false
        }
    }

    /// Redoes a previously undone edit, if any; returns whether the buffer changed.
    pub fn redo(&mut self) -> bool {
        if self.history.redo().is_some() {
            self.after_history_move();
            true
        } else {
            false
        }
    }

    // --- Cursor motion (each takes `extend` to grow or drop the selection) -----------

    /// Moves the cursor one character left.
    pub fn move_left(&mut self, extend: bool) {
        self.start_motion(extend);
        self.goal_column = None;
        let rope = self.rope();
        let chars = rope.byte_to_char(self.cursor);
        if chars > 0 {
            self.cursor = rope.char_to_byte(chars - 1);
        }
    }

    /// Moves the cursor one character right.
    pub fn move_right(&mut self, extend: bool) {
        self.start_motion(extend);
        self.goal_column = None;
        let rope = self.rope();
        let chars = rope.byte_to_char(self.cursor);
        if chars < rope.len_chars() {
            self.cursor = rope.char_to_byte(chars + 1);
        }
    }

    /// Moves the cursor up one line, preserving the goal column.
    pub fn move_up(&mut self, extend: bool) {
        self.start_motion(extend);
        let row = self.cursor_point().row;
        if row > 0 {
            self.move_to_row(row - 1);
        }
    }

    /// Moves the cursor down one line, preserving the goal column.
    pub fn move_down(&mut self, extend: bool) {
        self.start_motion(extend);
        let row = self.cursor_point().row;
        if row + 1 < self.rope().len_lines() {
            self.move_to_row(row + 1);
        }
    }

    /// Moves the cursor to the start of its line.
    pub fn move_line_start(&mut self, extend: bool) {
        self.start_motion(extend);
        self.goal_column = None;
        let row = self.cursor_point().row;
        self.cursor = self.line_start(row);
    }

    /// Moves the cursor to the end of its line.
    pub fn move_line_end(&mut self, extend: bool) {
        self.start_motion(extend);
        self.goal_column = None;
        let rope = self.rope();
        let row = rope.byte_to_point(self.cursor).row;
        let end_col = rope.line(row).len();
        self.cursor = rope.point_to_byte(Point::new(row, end_col));
    }

    /// Moves the cursor up to `rows` lines (Page Up), preserving the goal column.
    pub fn move_page_up(&mut self, rows: usize, extend: bool) {
        self.start_motion(extend);
        let row = self.cursor_point().row;
        self.move_to_row(row.saturating_sub(rows.max(1)));
    }

    /// Moves the cursor down up to `rows` lines (Page Down), preserving the goal column.
    pub fn move_page_down(&mut self, rows: usize, extend: bool) {
        self.start_motion(extend);
        let row = self.cursor_point().row;
        let last = self.rope().len_lines() - 1;
        self.move_to_row((row + rows.max(1)).min(last));
    }

    // --- Internals -------------------------------------------------------------------

    fn apply_edit(&mut self, range: Range<usize>, text: &str) {
        let start = range.start;
        let (next, edit) = self.rope().edit(range, text);
        self.history.record(next, edit);
        self.cursor = start + text.len();
        self.selection_anchor = None;
        self.goal_column = None;
        self.dirty = true;
    }

    fn after_history_move(&mut self) {
        self.cursor = snap_to_boundary(self.rope(), self.cursor);
        self.selection_anchor = None;
        self.goal_column = None;
        self.dirty = true;
    }

    fn start_motion(&mut self, extend: bool) {
        if extend {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
    }

    /// Moves to `row`, keeping (and updating) the goal column measured in `char`s.
    fn move_to_row(&mut self, row: usize) {
        let rope = self.rope();
        let goal = self.goal_column.unwrap_or_else(|| self.cursor_column());
        let line_chars = rope.line(row).chars().count();
        let target_col = goal.min(line_chars);
        let line_start_char = rope.byte_to_char(self.line_start(row));
        self.cursor = rope.char_to_byte(line_start_char + target_col);
        self.goal_column = Some(goal);
    }

    fn line_start(&self, row: usize) -> usize {
        self.rope().point_to_byte(Point::new(row, 0))
    }

    fn prev_boundary(&self, offset: usize) -> usize {
        let rope = self.rope();
        let chars = rope.byte_to_char(offset);
        if chars > 0 {
            rope.char_to_byte(chars - 1)
        } else {
            0
        }
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let rope = self.rope();
        let chars = rope.byte_to_char(offset);
        if chars < rope.len_chars() {
            rope.char_to_byte(chars + 1)
        } else {
            offset
        }
    }
}

/// Returns the largest `char` boundary `<= offset` (and `<= len`).
fn snap_to_boundary(rope: &Rope, offset: usize) -> usize {
    let mut offset = offset.min(rope.len_bytes());
    while offset > 0 && !rope.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::Buffer;

    #[test]
    fn insert_and_cursor_track() {
        let mut buffer = Buffer::scratch();
        buffer.insert("hello");
        assert_eq!(buffer.text(), "hello");
        assert_eq!(buffer.cursor(), 5);
        buffer.move_left(false);
        buffer.insert("X");
        assert_eq!(buffer.text(), "hellXo");
        assert_eq!(buffer.cursor(), 5);
    }

    #[test]
    fn backspace_and_delete_forward() {
        let mut buffer = Buffer::from_text("abc");
        buffer.move_line_end(false);
        buffer.backspace();
        assert_eq!(buffer.text(), "ab");
        buffer.move_line_start(false);
        buffer.delete_forward();
        assert_eq!(buffer.text(), "b");
        assert_eq!(buffer.cursor(), 0);
    }

    #[test]
    fn undo_redo_round_trip() {
        let mut buffer = Buffer::from_text("base");
        buffer.move_line_end(false);
        buffer.insert("!");
        assert_eq!(buffer.text(), "base!");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "base");
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "base!");
    }

    #[test]
    fn selection_then_replace_and_copy() {
        let mut buffer = Buffer::from_text("hello world");
        buffer.move_line_start(false);
        for _ in 0..5 {
            buffer.move_right(true); // select "hello"
        }
        assert_eq!(buffer.selected_text().as_deref(), Some("hello"));
        buffer.insert("HI"); // replaces the selection
        assert_eq!(buffer.text(), "HI world");
        assert!(buffer.selection().is_none());
    }

    #[test]
    fn vertical_motion_keeps_goal_column() {
        let mut buffer = Buffer::from_text("longline\nx\nlongline");
        buffer.move_line_start(false);
        for _ in 0..6 {
            buffer.move_right(false); // column 6 on the long first line
        }
        assert_eq!(buffer.cursor_column(), 6);
        buffer.move_down(false); // short line "x" -> clamps to column 1
        assert_eq!(buffer.cursor_column(), 1);
        buffer.move_down(false); // back to a long line -> goal column 6 restored
        assert_eq!(buffer.cursor_column(), 6);
    }

    #[test]
    fn multibyte_cursor_motion_stays_on_boundaries() {
        let mut buffer = Buffer::from_text("héllo");
        buffer.move_line_start(false);
        buffer.move_right(false); // past 'h'
        buffer.move_right(false); // past 'é' (2 bytes) — must land on a boundary
        assert_eq!(buffer.cursor(), 3); // 'h' (1) + 'é' (2)
        buffer.backspace(); // deletes 'é'
        assert_eq!(buffer.text(), "hllo");
    }

    #[test]
    fn open_missing_then_save_round_trip() {
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-core-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut buffer = Buffer::open(&path).unwrap();
        assert_eq!(buffer.text(), "");
        buffer.insert("saved content\n");
        assert!(buffer.is_dirty());
        buffer.save().unwrap();
        assert!(!buffer.is_dirty());

        let reopened = Buffer::open(&path).unwrap();
        assert_eq!(reopened.text(), "saved content\n");
        let _ = std::fs::remove_file(&path);
    }
}
