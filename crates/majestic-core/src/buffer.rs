// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The editable [`Buffer`] — a *view* over a shared [`Document`].
//!
//! A [`Document`] owns a [`stratum::UndoTree`] (whose current node is the live rope), the file
//! path, the crash-safe journal, and the dirty/revision flags — the *document* state. A
//! [`Buffer`] is a lightweight **view** holding an `Rc<RefCell<Document>>` plus per-view cursor,
//! selection anchor, and goal column. Multiple `Buffer`s can share one `Document` (two views of
//! one buffer): edits and undo are shared, while each view scrolls and places its cursor
//! independently. Every edit records a new undo node and is appended to the journal; opening a
//! file replays any journal a crashed session left behind (recovery), and saving checkpoints a
//! fresh journal.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use stratum::{replay, Anchor, Bias, Edit, EditOp, Journal, Point, Rope, UndoTree};

/// The shared document: content + undo history + file I/O + journal. Shared across views via
/// `Rc<RefCell<Document>>`; cursor/selection/viewport are per-view (see [`Buffer`]).
#[derive(Debug)]
pub struct Document {
    history: UndoTree,
    path: Option<PathBuf>,
    journal: Option<Journal>,
    journal_error: Option<io::Error>,
    recovered: bool,
    dirty: bool,
    revision: u64,
}

impl Document {
    fn from_rope(rope: Rope, path: Option<PathBuf>) -> Self {
        Self {
            history: UndoTree::new(rope),
            path,
            journal: None,
            journal_error: None,
            recovered: false,
            dirty: false,
            revision: 0,
        }
    }

    fn open(path: PathBuf) -> io::Result<Self> {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let base = Rope::from(text.as_str());
        let base_len = base.len_bytes() as u64;
        let jpath = journal_path(&path);

        let (rope, journal, recovered) = if jpath.exists() {
            let saved = Journal::recover(&jpath)?;
            if saved.base_len == base_len && !saved.ops.is_empty() {
                // A previous session left unsaved edits: replay them, keep appending.
                (
                    replay(&base, &saved.ops),
                    Journal::open_append(&jpath)?,
                    true,
                )
            } else {
                // Stale or empty journal: start fresh from the file content.
                (base, Journal::create(&jpath, base_len)?, false)
            }
        } else {
            (base, Journal::create(&jpath, base_len)?, false)
        };

        let mut doc = Self::from_rope(rope, Some(path));
        doc.journal = Some(journal);
        doc.recovered = recovered;
        doc.dirty = recovered; // recovered edits are unsaved
        Ok(doc)
    }

    fn save(&mut self) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer has no path",
            ));
        };
        fs::write(&path, self.history.current_rope().to_string())?;
        // Checkpoint: the file now holds the current content, so start a fresh journal.
        let base_len = self.history.current_rope().len_bytes() as u64;
        self.journal = Some(Journal::create(&journal_path(&path), base_len)?);
        self.journal_error = None;
        self.recovered = false;
        self.dirty = false;
        Ok(())
    }

    /// A cheap clone of the live rope (an `Arc` bump) — used in place of a `&Rope` borrow, which
    /// cannot outlive the `RefCell` guard.
    fn rope(&self) -> Rope {
        self.history.current_rope().clone()
    }

    fn apply_edit(&mut self, range: Range<usize>, text: &str) -> Edit {
        let start = range.start;
        let old_len = range.end - range.start;
        let (next, edit) = self.history.current_rope().edit(range, text);
        self.history.record(next, edit);
        if let Some(journal) = self.journal.as_mut() {
            if let Err(error) = journal.append(&EditOp::new(start, old_len, text)) {
                // Degrade gracefully: stop journaling and keep the error to surface.
                self.journal = None;
                self.journal_error = Some(error);
            }
        }
        self.dirty = true;
        self.revision += 1;
        edit // for sibling views to rebase their cursors across (Anchor tracking)
    }

    fn undo(&mut self) -> bool {
        if self.history.undo().is_some() {
            self.dirty = true;
            self.revision += 1;
            true
        } else {
            false
        }
    }

    fn redo(&mut self) -> bool {
        if self.history.redo().is_some() {
            self.dirty = true;
            self.revision += 1;
            true
        } else {
            false
        }
    }
}

/// A view over a shared [`Document`]: the document plus a per-view cursor, selection, and goal
/// column. Clone a view with [`Buffer::view`] to show one document in two panes.
#[derive(Clone, Debug)]
pub struct Buffer {
    doc: Rc<RefCell<Document>>,
    cursor: usize,
    selection_anchor: Option<usize>,
    goal_column: Option<usize>,
    /// The most recent edit this view applied, pending propagation to sibling views (see
    /// [`Buffer::take_last_edit`] / [`Buffer::shift_cursor`]). Forward edits only — undo/redo
    /// rely on clamp-on-access.
    last_edit: Option<Edit>,
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
        Self::wrap(Document::from_rope(rope, path))
    }

    /// Wraps a freshly-built document in a first view (cursor at the start).
    fn wrap(doc: Document) -> Self {
        Self {
            doc: Rc::new(RefCell::new(doc)),
            cursor: 0,
            selection_anchor: None,
            goal_column: None,
            last_edit: None,
        }
    }

    /// A second view of the same document: shares text + undo + journal, with an independent
    /// cursor (reset to the start) and no selection.
    #[must_use]
    pub fn view(&self) -> Self {
        Self {
            doc: Rc::clone(&self.doc),
            cursor: 0,
            selection_anchor: None,
            goal_column: None,
            last_edit: None,
        }
    }

    /// An opaque identity for this view's shared document — equal across views of the same
    /// document (used to scope cursor propagation to siblings).
    #[must_use]
    pub fn document_id(&self) -> usize {
        Rc::as_ptr(&self.doc).addr()
    }

    /// Takes the edit this view last applied (clearing it), for the host to propagate to sibling
    /// views via [`Buffer::shift_cursor`].
    pub fn take_last_edit(&mut self) -> Option<Edit> {
        self.last_edit.take()
    }

    /// Rebases this view's cursor and selection across an `edit` made in another view, so its
    /// logical position survives the shared-document change (Stratum [`Anchor`] semantics).
    pub fn shift_cursor(&mut self, edit: &Edit) {
        self.cursor = Anchor::new(self.cursor, Bias::Right).rebase(edit).offset();
        self.selection_anchor = self
            .selection_anchor
            .map(|anchor| Anchor::new(anchor, Bias::Left).rebase(edit).offset());
    }

    /// Opens `path`, reading its contents (an empty buffer if the file does not exist).
    ///
    /// # Errors
    /// Returns an I/O error if the file exists but cannot be read.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::wrap(Document::open(path.into())?))
    }

    /// Writes the buffer to its path and clears the dirty flag.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] if the buffer has no path, or any write error.
    pub fn save(&mut self) -> io::Result<()> {
        self.doc.borrow_mut().save()
    }

    // --- Queries ---------------------------------------------------------------------

    /// A cheap clone of the live rope (an `Arc` bump).
    #[must_use]
    pub fn rope(&self) -> Rope {
        self.doc.borrow().rope()
    }

    /// The buffer contents as a `String`.
    #[must_use]
    pub fn text(&self) -> String {
        self.doc.borrow().history.current_rope().to_string()
    }

    /// The cursor's byte offset (clamped to the current shared document — see [`Buffer::clamped`]).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.clamped(self.cursor)
    }

    /// Moves the cursor to `offset` (snapped to the nearest `char` boundary `<=` length), dropping
    /// any selection. Used to restore a saved session's cursor position.
    pub fn set_cursor(&mut self, offset: usize) {
        self.cursor = self.clamped(offset);
        self.selection_anchor = None;
        self.goal_column = None;
    }

    /// The cursor's `(row, byte-column)` position.
    #[must_use]
    pub fn cursor_point(&self) -> Point {
        let rope = self.rope();
        rope.byte_to_point(snap_to_boundary(&rope, self.cursor))
    }

    /// The cursor's column counted in `char`s from the start of its line (the display column).
    #[must_use]
    pub fn cursor_column(&self) -> usize {
        let rope = self.rope();
        let cursor = snap_to_boundary(&rope, self.cursor);
        let line_start = self.line_start(rope.byte_to_point(cursor).row);
        rope.byte_to_char(cursor) - rope.byte_to_char(line_start)
    }

    /// Whether the document has unsaved changes.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.doc.borrow().dirty
    }

    /// The document's file path, if any.
    #[must_use]
    pub fn path(&self) -> Option<PathBuf> {
        self.doc.borrow().path.clone()
    }

    /// A counter that increments on every content change (for cache invalidation).
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.doc.borrow().revision
    }

    /// Whether this document's content was recovered from a journal when opened.
    #[must_use]
    pub fn was_recovered(&self) -> bool {
        self.doc.borrow().recovered
    }

    /// The message of the error that disabled journaling, if one occurred.
    #[must_use]
    pub fn journal_error(&self) -> Option<String> {
        self.doc
            .borrow()
            .journal_error
            .as_ref()
            .map(io::Error::to_string)
    }

    /// The selected byte range, if a (non-empty) selection is active.
    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        let rope = self.rope();
        let cursor = snap_to_boundary(&rope, self.cursor);
        let anchor = snap_to_boundary(&rope, anchor);
        let (lo, hi) = (anchor.min(cursor), anchor.max(cursor));
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
        self.clamp();
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
        self.clamp();
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
        self.clamp();
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

    /// Kills the text from the cursor to the end of the line, returning it. If the cursor is
    /// already at the line end, the line break is killed instead (joining the next line) — the
    /// Emacs `C-k` `kill-line` semantics. The returned text is the caller's to push onto the kill
    /// ring / clipboard; an empty return means there was nothing to kill (cursor at buffer end).
    pub fn kill_line(&mut self) -> String {
        self.clamp();
        let rope = self.rope();
        // Mirror `move_line_end`: the end-of-line column excludes the trailing line break, so
        // `line_end` is the byte just before it.
        let row = rope.byte_to_point(self.cursor).row;
        let end_col = rope.line(row).len();
        let line_end = rope.point_to_byte(Point::new(row, end_col));
        let range = if self.cursor < line_end {
            self.cursor..line_end
        } else {
            self.cursor..self.next_boundary(self.cursor)
        };
        if range.is_empty() {
            return String::new();
        }
        let killed = rope.slice(range.clone());
        self.apply_edit(range, "");
        killed
    }

    /// Undoes the last edit, if any; returns whether the buffer changed.
    pub fn undo(&mut self) -> bool {
        if self.doc.borrow_mut().undo() {
            self.after_history_move();
            true
        } else {
            false
        }
    }

    /// Redoes a previously undone edit, if any; returns whether the buffer changed.
    pub fn redo(&mut self) -> bool {
        if self.doc.borrow_mut().redo() {
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
        // records undo, journals, bumps revision; the returned edit lets sibling views rebase.
        self.last_edit = Some(self.doc.borrow_mut().apply_edit(range, text));
        self.cursor = start + text.len();
        self.selection_anchor = None;
        self.goal_column = None;
    }

    fn after_history_move(&mut self) {
        let rope = self.rope();
        self.cursor = snap_to_boundary(&rope, self.cursor);
        self.selection_anchor = None;
        self.goal_column = None;
    }

    fn start_motion(&mut self, extend: bool) {
        self.clamp(); // a sibling view may have shrunk the shared document
        if extend {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
    }

    /// Snaps `offset` to the largest `char` boundary `<=` the current shared document's length.
    ///
    /// This is the design's Phase-1 clamp-on-access: a view's byte-offset cursor can fall out of
    /// range when another view shrinks the shared document, so reads clamp rather than panic.
    /// (Phase 2 promotes cursors to `stratum::Anchor`s, which track edits instead of clamping.)
    fn clamped(&self, offset: usize) -> usize {
        snap_to_boundary(&self.rope(), offset)
    }

    /// Clamps the cursor and selection anchor to the (possibly shrunk) shared document.
    fn clamp(&mut self) {
        let rope = self.rope();
        self.cursor = snap_to_boundary(&rope, self.cursor);
        self.selection_anchor = self
            .selection_anchor
            .map(|anchor| snap_to_boundary(&rope, anchor));
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

/// The sidecar journal path for a document: `<path>.mjjournal`.
fn journal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".mjjournal");
    PathBuf::from(name)
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
    fn views_share_text_and_undo() {
        let a = Buffer::from_text("hello");
        let mut b = a.view();
        b.move_line_end(false);
        b.insert("!"); // edit through the second view
        assert_eq!(
            a.text(),
            "hello!",
            "an edit in one view is visible in the other"
        );
        assert_eq!(b.text(), "hello!");

        let mut a = a;
        assert!(a.undo()); // undo through the first view affects the shared document
        assert_eq!(b.text(), "hello", "undo is shared across views");
    }

    #[test]
    fn views_have_independent_cursors() {
        let a = Buffer::from_text("hello");
        let mut b = a.view();
        b.move_line_end(false);
        assert_eq!(a.cursor(), 0, "the first view's cursor is untouched");
        assert_eq!(b.cursor(), 5);
        b.insert("X"); // edits the shared text; the other view's cursor stays put
        assert_eq!(a.text(), "helloX");
        assert_eq!(a.cursor(), 0);
    }

    #[test]
    fn view_cursor_clamps_when_a_sibling_shrinks_the_document() {
        let mut a = Buffer::from_text("hello");
        a.move_line_end(false); // a's cursor at byte 5
        let mut b = a.view();
        b.select_all();
        b.backspace(); // delete everything through b — the shared document is now empty
        assert_eq!(a.text(), "");
        assert_eq!(
            a.cursor(),
            0,
            "a's now-out-of-range cursor clamps to the new length"
        );
        a.move_left(false); // must not panic on the shrunk document
        assert_eq!(a.cursor(), 0);
    }

    #[test]
    fn sibling_cursor_tracks_an_edit_in_another_view() {
        let mut a = Buffer::from_text("hello");
        a.move_line_end(false); // a's cursor at byte 5
        let mut b = a.view();
        b.move_line_start(false);
        b.insert("AB"); // b inserts at the start of the shared document -> "ABhello"
        let edit = b.take_last_edit().expect("b recorded its edit");
        a.shift_cursor(&edit); // host propagates b's edit to a
        assert_eq!(a.text(), "ABhello");
        assert_eq!(
            a.cursor(),
            7,
            "a's cursor tracked the insertion before it (5 -> 7)"
        );
    }

    /// A unique temp document path and its journal sidecar, both removed up front.
    fn temp_doc(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-core-{tag}-{}.txt", std::process::id()));
        let mut journal = path.clone().into_os_string();
        journal.push(".mjjournal");
        let journal = std::path::PathBuf::from(journal);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
        (path, journal)
    }

    #[test]
    fn open_missing_then_save_round_trip() {
        let (path, journal) = temp_doc("save");
        let mut buffer = Buffer::open(&path).unwrap();
        assert_eq!(buffer.text(), "");
        buffer.insert("saved content\n");
        assert!(buffer.is_dirty());
        buffer.save().unwrap();
        assert!(!buffer.is_dirty());

        let reopened = Buffer::open(&path).unwrap();
        assert_eq!(reopened.text(), "saved content\n");
        assert!(!reopened.was_recovered());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
    }

    #[test]
    fn journal_recovers_unsaved_edits_after_crash() {
        let (path, journal) = temp_doc("crash");
        std::fs::write(&path, "base\n").unwrap();

        // Session 1: open, edit, never save, then drop — the record is already on disk,
        // so it survives a SIGKILL the same way (the kernel keeps written bytes).
        let mut buffer = Buffer::open(&path).unwrap();
        assert!(!buffer.was_recovered());
        buffer.move_line_end(false);
        buffer.insert("EDIT");
        assert!(buffer.is_dirty());
        drop(buffer); // close the journal file; the record persists on disk

        // Session 2: reopen — the unsaved edit is recovered from the journal.
        let recovered = Buffer::open(&path).unwrap();
        assert!(recovered.was_recovered());
        assert_eq!(recovered.text(), "baseEDIT\n");
        // The file was never written, so on disk it still holds the original content.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "base\n");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
    }

    /// The PRD §7 exit criterion: a real `SIGKILL` mid-edit loses no journaled keystrokes.
    ///
    /// The test re-spawns itself as a "victim" that opens the file, edits, signals readiness,
    /// then spins; the controller `SIGKILL`s it (via `Child::kill`) and asserts the reopened
    /// buffer recovers the edit from the journal.
    #[test]
    fn cross_process_sigkill_preserves_journaled_edits() {
        use std::process::{Command, Stdio};
        use std::time::Duration;

        // Victim role (re-spawned with the env var set): edit, signal, then spin (bounded).
        if let Ok(victim_path) = std::env::var("MJ_SIGKILL_VICTIM") {
            let mut buffer = Buffer::open(&victim_path).unwrap();
            buffer.move_line_end(false);
            buffer.insert("EDITED");
            std::fs::write(format!("{victim_path}.ready"), b"1").unwrap();
            for _ in 0..600 {
                std::thread::sleep(Duration::from_millis(50)); // up to 30s, then exit
            }
            return;
        }

        // Controller role.
        let (path, journal) = temp_doc("sigkill");
        std::fs::write(&path, "seed\n").unwrap();
        let ready = {
            let mut name = path.clone().into_os_string();
            name.push(".ready");
            std::path::PathBuf::from(name)
        };
        let _ = std::fs::remove_file(&ready);

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("cross_process_sigkill_preserves_journaled_edits")
            .env("MJ_SIGKILL_VICTIM", &path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let mut signalled = false;
        for _ in 0..500 {
            if ready.exists() {
                signalled = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(signalled, "victim never signalled readiness");

        child.kill().unwrap(); // SIGKILL on unix
        let _ = child.wait();

        let recovered = Buffer::open(&path).unwrap();
        assert!(recovered.was_recovered(), "no recovery after SIGKILL");
        assert!(
            recovered.text().contains("EDITED"),
            "edit lost across SIGKILL"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
        let _ = std::fs::remove_file(&ready);
    }
}
