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
use penumbra::{Buffer as Surface, Cell, Rect, Style, Theme};

use stratum::{Point, SpanLayer};

use crate::buffer::Buffer;
use crate::syntax::{HighlightKind, SyntaxHighlighter};

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
    highlighter: Option<SyntaxHighlighter>,
    highlights: SpanLayer<HighlightKind>,
    highlighted_revision: Option<u64>,
    tab_width: usize,
}

/// Default indent width in columns (CUA convention; overridden by `majestic-config`).
const DEFAULT_TAB_WIDTH: usize = 4;

impl Editor {
    /// Creates an editor on a scratch buffer with the CUA keymap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_buffer(Buffer::scratch())
    }

    /// Creates an editor on `buffer` with the CUA keymap.
    #[must_use]
    pub fn with_buffer(buffer: Buffer) -> Self {
        let highlighter = buffer.path().and_then(SyntaxHighlighter::for_path);
        Self {
            buffer,
            clipboard: String::new(),
            dispatcher: Dispatcher::new(vec![cua()]),
            viewport_top: 0,
            page_rows: 1,
            status: String::new(),
            quit: false,
            highlighter,
            highlights: SpanLayer::new(),
            highlighted_revision: None,
            tab_width: DEFAULT_TAB_WIDTH,
        }
    }

    /// Sets the indent width (columns), clamped to a sane `1..=16` range. Applied from config.
    pub fn set_tab_width(&mut self, width: usize) {
        self.tab_width = width.clamp(1, 16);
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

    /// Sets the status-line message (e.g. a startup notice from the host).
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
    }

    /// The clipboard contents.
    #[must_use]
    pub fn clipboard(&self) -> &str {
        &self.clipboard
    }

    /// Replaces the clipboard contents (used to mirror one shared kill-ring across panes).
    pub fn set_clipboard(&mut self, text: &str) {
        self.clipboard.clear();
        self.clipboard.push_str(text);
    }

    /// The buffer's display name: its file name, or `[scratch]` for an unsaved buffer.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.buffer
            .path()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("[scratch]")
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
            "indent" => self.buffer.insert(&" ".repeat(self.tab_width)),
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

    /// Draws the buffer and cursor over the whole `surface`, with a status line on the last row.
    ///
    /// This is the standalone full-screen path; [`Editor::render_in`] draws into a sub-region
    /// (e.g. above an integrated terminal panel) without claiming a status row.
    pub fn render(&mut self, surface: &mut Surface, theme: &Theme) {
        let height = surface.height();
        if surface.width() == 0 || height == 0 {
            return;
        }
        let (content, status) = surface.area().split_bottom(1);
        self.render_in(surface, content, theme, true);
        self.draw_status(surface, theme, status.y);
    }

    /// Draws the buffer within `area`, optionally drawing the cursor (when this pane is focused).
    ///
    /// Writes are offset to `area`'s origin and clipped to its extent, so the editor can occupy
    /// any sub-rectangle of the screen. No status line is drawn — the host composes that.
    pub fn render_in(&mut self, surface: &mut Surface, area: Rect, theme: &Theme, focused: bool) {
        if area.is_empty() {
            return;
        }
        self.page_rows = usize::from(area.height).max(1);
        self.refresh_highlights();
        self.ensure_cursor_visible(area.height);
        surface.fill(area, theme.base_style());

        let base = theme.base_style();
        let rope = self.buffer.rope();
        for row in 0..area.height {
            let line_index = self.viewport_top + usize::from(row);
            if line_index >= rope.len_lines() {
                break;
            }
            let mut byte = rope.point_to_byte(Point::new(line_index, 0));
            for (index, ch) in rope.line(line_index).chars().enumerate() {
                let Ok(col) = u16::try_from(index) else {
                    break;
                };
                if col >= area.width {
                    break;
                }
                surface.set_char(
                    area.x + col,
                    area.y + row,
                    ch,
                    self.style_at(byte, base, theme),
                );
                byte += ch.len_utf8();
            }
        }

        if focused {
            self.draw_cursor(surface, theme, area);
        }
    }

    /// Re-runs the highlighter when the buffer has changed since the last highlight.
    fn refresh_highlights(&mut self) {
        let revision = self.buffer.revision();
        if self.highlighted_revision == Some(revision) {
            return;
        }
        self.highlighted_revision = Some(revision);
        if self.highlighter.is_some() {
            let text = self.buffer.text();
            if let Some(highlighter) = self.highlighter.as_mut() {
                self.highlights = highlighter.highlight(text.as_bytes());
            }
        }
    }

    /// The style for the cell at byte `offset`: its highlight, or the base style.
    fn style_at(&self, offset: usize, base: Style, theme: &Theme) -> Style {
        self.highlights
            .spans_in(offset..offset + 1)
            .next()
            .map_or(base, |span| span.value.style(theme))
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

    fn draw_cursor(&self, surface: &mut Surface, theme: &Theme, area: Rect) {
        let row = self.buffer.cursor_point().row;
        if row < self.viewport_top {
            return;
        }
        let screen_row = row - self.viewport_top;
        let column = self.buffer.cursor_column();
        let (Ok(cx), Ok(cy)) = (u16::try_from(column), u16::try_from(screen_row)) else {
            return;
        };
        if cy >= area.height || cx >= area.width {
            return;
        }
        let (x, y) = (area.x + cx, area.y + cy);
        let (symbol, mut style) = surface
            .cell(x, y)
            .map_or((' ', theme.base_style()), |cell| (cell.symbol, cell.style));
        style.attrs.reverse = true;
        surface.set(x, y, Cell::new(symbol, style));
    }

    /// The status-line text: file name, dirty marker, cursor position, and last status message.
    ///
    /// The host composes this into its status bar (the standalone [`Editor::render`] draws it on
    /// the bottom row; the `mj` app folds it into a global status bar alongside a focus hint).
    #[must_use]
    pub fn status_line(&self) -> String {
        let point = self.buffer.cursor_point();
        let dirty = if self.buffer.is_dirty() { " *" } else { "" };
        format!(
            " {}{dirty}   Ln {}, Col {}   {}",
            self.display_name(),
            point.row + 1,
            self.buffer.cursor_column() + 1,
            self.status,
        )
    }

    fn draw_status(&self, surface: &mut Surface, theme: &Theme, row: u16) {
        let line = self.status_line();
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
    fn indent_uses_the_configured_tab_width() {
        let mut editor = Editor::new();
        editor.set_tab_width(2);
        editor.execute("indent");
        assert_eq!(editor.buffer().text(), "  ");
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

    #[test]
    fn render_applies_syntax_highlighting() {
        // Opening a `.rs` file attaches a tree-sitter highlighter; rendering must then style
        // the `fn` keyword with the theme accent (UI.md §3), not the default foreground.
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-hl-{}.rs", std::process::id()));
        let mut journal = path.clone().into_os_string();
        journal.push(".mjjournal");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let theme = Theme::steelbore();
        let mut editor = Editor::with_buffer(Buffer::open(&path).unwrap());
        let mut surface = Surface::new(20, 3, theme.base_style());
        editor.render(&mut surface, &theme);

        // `n` of the `fn` keyword (col 1, no cursor) is drawn in the accent color.
        let cell = surface.cell(1, 0).unwrap();
        assert_eq!(cell.symbol, 'n');
        assert_eq!(cell.style.fg, theme.accent);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
    }
}
