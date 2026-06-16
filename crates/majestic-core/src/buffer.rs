// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The editable [`Buffer`] — a document model over Stratum's rope and undo tree.
//!
//! A [`Buffer`] owns a [`stratum::UndoTree`] (whose current node is the live rope), a byte
//! cursor kept on a `char` boundary, an optional selection anchor, and a goal column for
//! vertical motion. Every edit goes through one path that records a new undo node and moves
//! the cursor, so undo/redo and editing stay consistent. Each edit is appended to a
//! crash-safe journal; opening a file replays any journal a crashed session left behind
//! (recovery), and saving checkpoints a fresh journal.

use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use stratum::{replay, EditOp, Journal, Point, Rope, UndoTree};

/// An editable text document: content, cursor, selection, and undo history.
#[derive(Debug)]
pub struct Buffer {
    history: UndoTree,
    cursor: usize,
    selection_anchor: Option<usize>,
    goal_column: Option<usize>,
    path: Option<PathBuf>,
    journal: Option<Journal>,
    journal_error: Option<io::Error>,
    recovered: bool,
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
            journal: None,
            journal_error: None,
            recovered: false,
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

        let mut buffer = Self::from_rope(rope, Some(path));
        buffer.journal = Some(journal);
        buffer.recovered = recovered;
        buffer.dirty = recovered; // recovered edits are unsaved
        Ok(buffer)
    }

    /// Writes the buffer to its path and clears the dirty flag.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] if the buffer has no path, or any write error.
    pub fn save(&mut self) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer has no path",
            ));
        };
        fs::write(&path, self.text())?;
        // Checkpoint: the file now holds the current content, so start a fresh journal.
        let base_len = self.rope().len_bytes() as u64;
        self.journal = Some(Journal::create(&journal_path(&path), base_len)?);
        self.journal_error = None;
        self.recovered = false;
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

    /// Whether this buffer's content was recovered from a journal when opened.
    #[must_use]
    pub fn was_recovered(&self) -> bool {
        self.recovered
    }

    /// The error that disabled journaling, if one occurred.
    #[must_use]
    pub fn journal_error(&self) -> Option<&io::Error> {
        self.journal_error.as_ref()
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
        let old_len = range.end - range.start;
        let (next, edit) = self.rope().edit(range, text);
        self.history.record(next, edit);
        if let Some(journal) = self.journal.as_mut() {
            if let Err(error) = journal.append(&EditOp::new(start, old_len, text)) {
                // Degrade gracefully: stop journaling and keep the error to surface.
                self.journal = None;
                self.journal_error = Some(error);
            }
        }
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
