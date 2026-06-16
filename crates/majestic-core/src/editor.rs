// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`Editor`] — composes a [`Buffer`], the keymap [`Dispatcher`], a clipboard, and a
//! viewport into the interactive editing model.
//!
//! [`Editor::handle_key`] feeds a key to the dispatcher and either runs the resolved command
//! ([`Editor::execute`]) or self-inserts an unclaimed printable key. [`Editor::render`] draws
//! the visible lines, a reverse-video cursor cell, and a status line into a Penumbra buffer.
//! The interactive `crossterm` loop and the `mj FILE` binary wire this up in M0 step 7.

use keymaker::{cua, Dispatcher, KeyCode, Mods, Resolution};
use penumbra::{Buffer as Surface, Cell, Style, Theme};

use crate::buffer::Buffer;

/// The editing session: a buffer, its keymaps, a clipboard, and the visible viewport.
#[derive(Debug)]
pub struct Editor {
    buffer: Buffer,
    clipboard: String,
    dispatcher: Dispatcher,
    viewport_top: usize,
    page_rows: usize,
    status: String,
    quit: bool,
}

impl Editor {
    /// Creates an editor on a scratch buffer with the CUA keymap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_buffer(Buffer::scratch())
    }

    /// Creates an editor on `buffer` with the CUA keymap.
    #[must_use]
    pub fn with_buffer(buffer: Buffer) -> Self {
        Self {
            buffer,
            clipboard: String::new(),
            dispatcher: Dispatcher::new(vec![cua()]),
            viewport_top: 0,
            page_rows: 1,
            status: String::new(),
            quit: false,
        }
    }

    /// The active buffer.
    #[must_use]
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// The active buffer, mutably.
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }

    /// Whether a quit command has been issued.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The current status-line message.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The clipboard contents.
    #[must_use]
    pub fn clipboard(&self) -> &str {
        &self.clipboard
    }

    /// Feeds one key: runs the resolved command, waits on a prefix, or self-inserts.
    pub fn handle_key(&mut self, key: keymaker::KeyPress) -> Resolution {
        let resolution = self.dispatcher.feed(key);
        match &resolution {
            Resolution::Command(command) => self.execute(command.name()),
            Resolution::Pending => {}
            Resolution::Unbound(chord) => {
                if let [press] = chord.as_slice() {
                    if let KeyCode::Char(ch) = press.code {
                        let modified = press.mods.contains(Mods::CTRL)
                            || press.mods.contains(Mods::ALT)
                            || press.mods.contains(Mods::SUPER);
                        if !modified {
                            self.self_insert(ch);
                        }
                    }
                }
            }
        }
        resolution
    }

    /// Inserts a character at the cursor (used for unbound printable keys).
    pub fn self_insert(&mut self, ch: char) {
        self.buffer.insert_char(ch);
    }

    /// Runs the named command against the editor state.
    pub fn execute(&mut self, command: &str) {
        match command {
            "move-left" => self.buffer.move_left(false),
            "move-right" => self.buffer.move_right(false),
            "move-up" => self.buffer.move_up(false),
            "move-down" => self.buffer.move_down(false),
            "move-line-start" => self.buffer.move_line_start(false),
            "move-line-end" => self.buffer.move_line_end(false),
            "page-up" => self.buffer.move_page_up(self.page_rows, false),
            "page-down" => self.buffer.move_page_down(self.page_rows, false),
            "select-left" => self.buffer.move_left(true),
            "select-right" => self.buffer.move_right(true),
            "select-up" => self.buffer.move_up(true),
            "select-down" => self.buffer.move_down(true),
            "select-all" => self.buffer.select_all(),
            "delete-backward" => self.buffer.backspace(),
            "delete-forward" => self.buffer.delete_forward(),
            "insert-newline" => self.buffer.insert("\n"),
            "indent" => self.buffer.insert("    "),
            "undo" => {
                self.buffer.undo();
            }
            "redo" => {
                self.buffer.redo();
            }
            "copy" => self.copy(),
            "cut" => self.cut(),
            "paste" => self.buffer.insert(&self.clipboard),
            "save" => self.save(),
            "find" => "find: not yet implemented (M1)".clone_into(&mut self.status),
            "quit" | "close-buffer" => self.quit = true,
            other => self.status = format!("unbound command: {other}"),
        }
    }

    fn copy(&mut self) {
        if let Some(text) = self.buffer.selected_text() {
            self.clipboard = text;
            "copied".clone_into(&mut self.status);
        }
    }

    fn cut(&mut self) {
        if let Some(text) = self.buffer.selected_text() {
            self.clipboard = text;
            self.buffer.delete_selection();
            "cut".clone_into(&mut self.status);
        }
    }

    fn save(&mut self) {
        match self.buffer.save() {
            Ok(()) => "saved".clone_into(&mut self.status),
            Err(error) => self.status = format!("save failed: {error}"),
        }
    }

    /// Draws the buffer, cursor, and status line into `surface`.
    pub fn render(&mut self, surface: &mut Surface, theme: &Theme) {
        let (width, height) = (surface.width(), surface.height());
        if width == 0 || height == 0 {
            return;
        }
        let text_rows = height.saturating_sub(1);
        self.page_rows = usize::from(text_rows).max(1);
        self.ensure_cursor_visible(text_rows);
        surface.clear(theme.base_style());

        let rope = self.buffer.rope();
        for row in 0..text_rows {
            let line_index = self.viewport_top + usize::from(row);
            if line_index >= rope.len_lines() {
                break;
            }
            surface.set_str(0, row, &rope.line(line_index), theme.base_style());
        }

        self.draw_cursor(surface, theme, text_rows);
        self.draw_status(surface, theme, height - 1);
    }

    fn ensure_cursor_visible(&mut self, text_rows: u16) {
        let rows = usize::from(text_rows);
        if rows == 0 {
            return;
        }
        let row = self.buffer.cursor_point().row;
        if row < self.viewport_top {
            self.viewport_top = row;
        } else if row >= self.viewport_top + rows {
            self.viewport_top = row + 1 - rows;
        }
    }

    fn draw_cursor(&self, surface: &mut Surface, theme: &Theme, text_rows: u16) {
        let row = self.buffer.cursor_point().row;
        if row < self.viewport_top {
            return;
        }
        let screen_row = row - self.viewport_top;
        let column = self.buffer.cursor_column();
        let (Ok(cx), Ok(cy)) = (u16::try_from(column), u16::try_from(screen_row)) else {
            return;
        };
        if cy >= text_rows || cx >= surface.width() {
            return;
        }
        let (symbol, mut style) = surface
            .cell(cx, cy)
            .map_or((' ', theme.base_style()), |cell| (cell.symbol, cell.style));
        style.attrs.reverse = true;
        surface.set(cx, cy, Cell::new(symbol, style));
    }

    fn draw_status(&self, surface: &mut Surface, theme: &Theme, row: u16) {
        let point = self.buffer.cursor_point();
        let name = self
            .buffer
            .path()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("[scratch]");
        let dirty = if self.buffer.is_dirty() { " *" } else { "" };
        let line = format!(
            " {name}{dirty}   Ln {}, Col {}   {}",
            point.row + 1,
            self.buffer.cursor_column() + 1,
            self.status,
        );
        let style = Style::new(theme.background, theme.accent);
        for x in 0..surface.width() {
            surface.set_char(x, row, ' ', style);
        }
        surface.set_str(0, row, &line, style);
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Editor;
    use crate::buffer::Buffer;
    use keymaker::{KeyCode, KeyPress};
    use penumbra::{Buffer as Surface, Theme};

    #[test]
    fn arrow_keys_move_the_cursor() {
        let mut editor = Editor::with_buffer(Buffer::from_text("ab\ncd"));
        editor.handle_key(KeyPress::key(KeyCode::Down));
        editor.handle_key(KeyPress::key(KeyCode::Right));
        assert_eq!(editor.buffer().cursor_point().row, 1);
        assert_eq!(editor.buffer().cursor_column(), 1);
    }

    #[test]
    fn unbound_printable_key_self_inserts() {
        let mut editor = Editor::new();
        editor.handle_key(KeyPress::char('h'));
        editor.handle_key(KeyPress::char('i'));
        assert_eq!(editor.buffer().text(), "hi");
    }

    #[test]
    fn ctrl_z_undoes() {
        let mut editor = Editor::new();
        editor.handle_key(KeyPress::char('x'));
        assert_eq!(editor.buffer().text(), "x");
        editor.handle_key(KeyPress::ctrl('z'));
        assert_eq!(editor.buffer().text(), "");
    }

    #[test]
    fn copy_and_paste_via_clipboard() {
        let mut editor = Editor::with_buffer(Buffer::from_text("word"));
        editor.handle_key(KeyPress::key(KeyCode::Home));
        for _ in 0..4 {
            editor.handle_key(KeyPress::new(keymaker::Mods::SHIFT, KeyCode::Right));
        }
        editor.handle_key(KeyPress::ctrl('c')); // copy "word"
        assert_eq!(editor.clipboard(), "word");
        editor.handle_key(KeyPress::key(KeyCode::End));
        editor.handle_key(KeyPress::ctrl('v')); // paste at end
        assert_eq!(editor.buffer().text(), "wordword");
    }

    #[test]
    fn quit_command_sets_flag() {
        let mut editor = Editor::new();
        assert!(!editor.should_quit());
        editor.handle_key(KeyPress::ctrl('q'));
        assert!(editor.should_quit());
    }

    #[test]
    fn render_draws_lines_cursor_and_status() {
        let theme = Theme::steelbore();
        let mut editor = Editor::with_buffer(Buffer::from_text("hello\nworld"));
        let mut surface = Surface::new(20, 3, theme.base_style());
        editor.render(&mut surface, &theme);

        assert_eq!(surface.cell(0, 0).unwrap().symbol, 'h');
        assert_eq!(surface.cell(0, 1).unwrap().symbol, 'w');
        // Cursor starts at (0,0) and is drawn in reverse video.
        assert!(surface.cell(0, 0).unwrap().style.attrs.reverse);
        // Status line (row 2) is non-blank and reports line 1.
        assert_ne!(surface.cell(1, 2).unwrap().symbol, ' ');
    }
}
