// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`Editor`] — composes a [`Buffer`], the keymap [`Dispatcher`], a clipboard, and a
//! viewport into the interactive editing model.
//!
//! [`Editor::handle_key`] feeds a key to the dispatcher and either runs the resolved command
//! ([`Editor::execute`]) or self-inserts an unclaimed printable key. [`Editor::render`] draws
//! the visible lines, a reverse-video cursor cell, and a status line into a Penumbra buffer.
//! The interactive `crossterm` loop and the `mj FILE` binary wire this up in M0 step 7.

use std::path::Path;

use keymaker::{
    cua, emacs, spacemacs_normal, vim_insert, vim_normal, vim_visual, Continuation, Dispatcher,
    KeyCode, KeyPress, Keymap, Mods, Profile, Resolution,
};
use penumbra::{char_width, Buffer as Surface, Cell, Rect, Rgb, Style, Theme};

use stratum::{Point, SpanLayer};

use crate::buffer::Buffer;
use crate::diagnostic::{Diagnostic, Severity};
use crate::syntax::{HighlightKind, HighlightWorker};

/// The editing mode that governs key dispatch and whether printable keys self-insert.
///
/// Non-modal profiles (CUA, Emacs) stay in [`EditMode::Insert`]. The Vim profile cycles
/// `Normal`/`Insert`/`Visual`; only `Insert` self-inserts an unclaimed printable key, so Normal-
/// and Visual-mode keystrokes never leak into the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditMode {
    /// Keys insert text — the default for non-modal profiles and for Vim insert mode.
    Insert,
    /// Vim Normal mode: motion and operators; printable keys do not self-insert.
    Normal,
    /// Vim Visual mode: motion extends the selection; printable keys do not self-insert.
    Visual,
}

impl EditMode {
    /// Whether an unclaimed printable key is inserted into the buffer in this mode.
    #[must_use]
    pub const fn inserts_text(self) -> bool {
        matches!(self, Self::Insert)
    }

    /// A short uppercase label for the status line (`INSERT`, `NORMAL`, `VISUAL`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Normal => "NORMAL",
            Self::Visual => "VISUAL",
        }
    }
}

/// The three mode keymaps an [`Editor`] swaps between while a modal profile (Vim or Spacemacs)
/// is active. Insert and Visual are shared; only the Normal map differs between the two.
#[derive(Clone, Debug)]
struct ModalKeymaps {
    normal: Keymap,
    insert: Keymap,
    visual: Keymap,
}

/// The editing session: a buffer, its keymaps, a clipboard, and the visible viewport.
#[derive(Debug)]
pub struct Editor {
    buffer: Buffer,
    clipboard: String,
    dispatcher: Dispatcher,
    mode: EditMode,
    modal: Option<ModalKeymaps>,
    viewport_top: usize,
    viewport_left: usize,
    page_rows: usize,
    status: String,
    quit: bool,
    highlighter: Option<HighlightWorker>,
    highlights: SpanLayer<HighlightKind>,
    highlighted_revision: Option<u64>,
    requested_revision: Option<u64>,
    tab_width: usize,
    /// Language-server diagnostics for this buffer (byte ranges), underlined when rendered.
    diagnostics: Vec<Diagnostic>,
}

/// Default indent width in columns (CUA convention; overridden by `majestic-config`).
const DEFAULT_TAB_WIDTH: usize = 4;

/// Ranks a severity so the most serious sorts first (for `min_by_key`).
fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Information => 2,
        Severity::Hint => 3,
    }
}

/// The Steelbore palette color for a severity (§9): error = Red Oxide, warning = Molten Amber,
/// info/hint = Liquid Coolant.
fn severity_color(severity: Severity, theme: &Theme) -> Rgb {
    match severity {
        Severity::Error => theme.error,
        Severity::Warning => theme.foreground,
        Severity::Information | Severity::Hint => theme.info,
    }
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
        let highlighter = buffer.path().as_deref().and_then(HighlightWorker::for_path);
        Self {
            buffer,
            clipboard: String::new(),
            dispatcher: Dispatcher::new(vec![cua()]),
            mode: EditMode::Insert,
            modal: None,
            viewport_top: 0,
            viewport_left: 0,
            page_rows: 1,
            status: String::new(),
            quit: false,
            highlighter,
            highlights: SpanLayer::new(),
            highlighted_revision: None,
            requested_revision: None,
            tab_width: DEFAULT_TAB_WIDTH,
            diagnostics: Vec::new(),
        }
    }

    /// Replaces this buffer's diagnostics (the host sets these from the language server's
    /// `publishDiagnostics`). They are underlined on the next render until superseded.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<Diagnostic>) {
        self.diagnostics = diagnostics;
    }

    /// The diagnostics currently shown for this buffer.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// A second view of this editor's buffer: a new editor sharing the document (and thus text +
    /// undo) with an independent cursor, viewport, and highlighter. Used to show one buffer in
    /// two panes.
    #[must_use]
    pub fn view(&self) -> Self {
        let mut editor = Self::with_buffer(self.buffer.view());
        editor.set_tab_width(self.tab_width);
        editor
    }

    /// Sets the indent width (columns), clamped to a sane `1..=16` range. Applied from config.
    pub fn set_tab_width(&mut self, width: usize) {
        self.tab_width = width.clamp(1, 16);
    }

    /// The viewport scroll offset as `(top_row, left_column)`. Captured into a saved session.
    #[must_use]
    pub fn viewport(&self) -> (usize, usize) {
        (self.viewport_top, self.viewport_left)
    }

    /// Restores a saved session's cursor and viewport. The cursor is clamped to the buffer; the
    /// viewport is reconciled with the cursor on the next render (`scroll_into_view`).
    pub fn restore_position(&mut self, cursor: usize, viewport_top: usize, viewport_left: usize) {
        self.buffer.set_cursor(cursor);
        self.viewport_top = viewport_top;
        self.viewport_left = viewport_left;
    }

    /// Switches this editor to `profile`, live. Dispatch is synchronous (one key is fully handled
    /// before the next), so swapping the keymap between keystrokes drops nothing in flight — the
    /// "profile switch under load loses no keystrokes" guarantee (PRD §8).
    pub fn set_profile(&mut self, profile: Profile) {
        match profile {
            Profile::Cua => self.set_non_modal(cua()),
            Profile::Emacs => self.set_non_modal(emacs()),
            Profile::Vim => self.enable_vim(),
            Profile::Spacemacs => self.enable_spacemacs(),
        }
    }

    /// Installs a single-layer, non-modal keymap (CUA, Emacs): always [`EditMode::Insert`], no
    /// modal state. The new keymap shares structure with the old via `Arc`, so this is cheap.
    fn set_non_modal(&mut self, keymap: Keymap) {
        self.modal = None;
        self.mode = EditMode::Insert;
        self.dispatcher = Dispatcher::new(vec![keymap]);
    }

    /// Switches this editor to the Vim profile, starting in Normal mode; installs the three modal
    /// keymaps. Prefer [`Editor::set_profile`] at call sites that select by [`Profile`].
    pub fn enable_vim(&mut self) {
        self.set_modal(vim_normal());
    }

    /// Switches this editor to the Spacemacs profile (Vim modality + a `SPC` leader in Normal
    /// mode), starting in Normal mode.
    pub fn enable_spacemacs(&mut self) {
        self.set_modal(spacemacs_normal());
    }

    /// Installs a modal profile with `normal` as the Normal-mode keymap (Insert/Visual are the
    /// shared Vim maps) and enters Normal mode.
    fn set_modal(&mut self, normal: Keymap) {
        self.modal = Some(ModalKeymaps {
            normal,
            insert: vim_insert(),
            visual: vim_visual(),
        });
        self.set_mode(EditMode::Normal);
    }

    /// The current editing mode (always [`EditMode::Insert`] for non-modal profiles).
    #[must_use]
    pub fn mode(&self) -> EditMode {
        self.mode
    }

    /// The which-key hint rows for the in-progress key prefix: each `(key, label)` pairs the next
    /// key with the command it runs (or `+prefix` when it descends further). Empty when no
    /// multi-key sequence is pending, so the host shows the hint only mid-chord.
    #[must_use]
    pub fn which_key(&self) -> Vec<(KeyPress, String)> {
        self.dispatcher
            .continuations()
            .into_iter()
            .map(|(key, continuation)| {
                let label = match continuation {
                    Continuation::Command(command) => command.name().to_owned(),
                    Continuation::Prefix => "+prefix".to_owned(),
                };
                (key, label)
            })
            .collect()
    }

    /// In a modal editor, switches mode and rebinds the dispatcher to that mode's keymap. A no-op
    /// for non-modal profiles. Rebinding is a cheap `Arc` clone (the prefix tree shares
    /// structure), so it never disturbs dispatch.
    fn set_mode(&mut self, mode: EditMode) {
        let Some(modal) = &self.modal else { return };
        let keymap = match mode {
            EditMode::Insert => modal.insert.clone(),
            EditMode::Normal => modal.normal.clone(),
            EditMode::Visual => modal.visual.clone(),
        };
        self.dispatcher = Dispatcher::new(vec![keymap]);
        self.mode = mode;
        self.status = format!("-- {} --", mode.label());
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
    pub fn display_name(&self) -> String {
        self.buffer
            .path()
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map_or_else(|| "[scratch]".to_owned(), str::to_owned)
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
                        // Only insert in a mode that takes text — Vim Normal/Visual swallow the key.
                        if !modified && self.mode.inserts_text() {
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
            "kill-line" => self.kill_line(),
            "paste" => self.buffer.insert(&self.clipboard),
            "enter-insert-mode" => self.set_mode(EditMode::Insert),
            "enter-normal-mode" => self.set_mode(EditMode::Normal),
            "enter-visual-mode" => self.set_mode(EditMode::Visual),
            "profile-cua" => self.set_profile(Profile::Cua),
            "profile-emacs" => self.set_profile(Profile::Emacs),
            "profile-vim" => self.set_profile(Profile::Vim),
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

    /// Emacs `kill-line` (`C-k`): kills from the cursor to the line end (or the line break) and
    /// puts the killed text on the clipboard so `paste` (`C-y` yank) restores it.
    fn kill_line(&mut self) {
        let killed = self.buffer.kill_line();
        if !killed.is_empty() {
            self.clipboard = killed;
            "killed line".clone_into(&mut self.status);
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
        self.ensure_cursor_visible(area);
        surface.fill(area, theme.base_style());

        let base = theme.base_style();
        let rope = self.buffer.rope();
        let first_line = self.viewport_top;
        let start_byte = rope.point_to_byte(Point::new(first_line, 0));
        let last_line = (first_line + usize::from(area.height)).min(rope.len_lines());
        let end_byte = if last_line >= rope.len_lines() {
            rope.len_bytes()
        } else {
            rope.point_to_byte(Point::new(last_line, 0))
        };
        let styles = self.visible_styles(start_byte, end_byte, base, theme);

        for row in 0..area.height {
            let line_index = first_line + usize::from(row);
            if line_index >= rope.len_lines() {
                break;
            }
            let mut byte = rope.point_to_byte(Point::new(line_index, 0));
            let mut display = 0usize; // absolute display column within the line
            for ch in rope.line(line_index).chars() {
                if display >= self.viewport_left {
                    let screen = display - self.viewport_left;
                    if screen >= usize::from(area.width) {
                        break; // the rest of the line is off the right edge
                    }
                    if let Ok(col) = u16::try_from(screen) {
                        let style = styles.get(byte - start_byte).copied().unwrap_or(base);
                        let style = self.apply_diagnostic(byte, style, theme);
                        surface.set_char(area.x + col, area.y + row, ch, style);
                    }
                }
                // Glyphs left of the viewport (or a wide glyph straddling its left edge) are
                // skipped — only their width is accounted for.
                display += usize::from(char_width(ch));
                byte += ch.len_utf8();
            }
        }

        if focused {
            self.draw_cursor(surface, theme, area);
        }
    }

    /// If `byte` is covered by a diagnostic, returns `style` underlined in the diagnostic's
    /// severity color (the most severe covering diagnostic wins); otherwise `style` unchanged.
    fn apply_diagnostic(&self, byte: usize, mut style: Style, theme: &Theme) -> Style {
        if self.diagnostics.is_empty() {
            return style;
        }
        let severity = self
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.covers(byte))
            .map(|diagnostic| diagnostic.severity)
            .min_by_key(|severity| severity_rank(*severity));
        if let Some(severity) = severity {
            style.fg = severity_color(severity, theme);
            style.attrs.underline = true;
        }
        style
    }

    /// The most severe diagnostic covering the cursor, if any (shown in the status line).
    #[must_use]
    pub fn cursor_diagnostic(&self) -> Option<&Diagnostic> {
        let cursor = self.buffer.cursor();
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.covers(cursor))
            .min_by_key(|diagnostic| severity_rank(diagnostic.severity))
    }

    /// Reconciles highlights with the background worker — never blocks the render path.
    ///
    /// Applies any finished result (newest wins) and, if the buffer has changed since the last
    /// request, sends a fresh snapshot (a cheap `Rope` clone). A frame paints with whatever
    /// highlights are current; during fast typing they trail by a frame or two and then catch up.
    fn refresh_highlights(&mut self) {
        if let Some(done) = self.highlighter.as_ref().and_then(HighlightWorker::poll) {
            self.highlights = done.layer;
            self.highlighted_revision = Some(done.revision);
        }
        let revision = self.buffer.revision();
        if self.requested_revision != Some(revision) {
            if let Some(worker) = self.highlighter.as_ref() {
                worker.request(revision, self.buffer.rope());
            }
            self.requested_revision = Some(revision);
        }
    }

    /// Blocks until highlights reflect the current buffer revision (a no-op without a worker).
    ///
    /// For deterministic, non-interactive rendering — tests and the perf harness — where the
    /// frame must show finished highlights rather than the asynchronous steady state.
    pub fn flush_highlights(&mut self) {
        let revision = self.buffer.revision();
        if self.requested_revision != Some(revision) {
            if let Some(worker) = self.highlighter.as_ref() {
                worker.request(revision, self.buffer.rope());
            }
            self.requested_revision = Some(revision);
        }
        if let Some(done) = self
            .highlighter
            .as_ref()
            .and_then(|worker| worker.wait_for(revision))
        {
            self.highlights = done.layer;
            self.highlighted_revision = Some(done.revision);
        }
    }

    /// Precomputes a per-byte [`Style`] array for the visible byte range `[start, end)` in a
    /// single pass over the highlight spans.
    ///
    /// This replaces a per-glyph span scan: rendering a frame is then `O(glyphs + spans)` rather
    /// than `O(glyphs × spans)` — the hot path the §7 harness flagged (keypress/scroll p99 went
    /// from hundreds of milliseconds to well under one frame).
    fn visible_styles(&self, start: usize, end: usize, base: Style, theme: &Theme) -> Vec<Style> {
        let len = end.saturating_sub(start);
        let mut styles = vec![base; len];
        if len == 0 {
            return styles;
        }
        for span in self.highlights.spans_in(start..end) {
            let style = span.value.style(theme);
            let range = span.range();
            let from = range.start.max(start) - start;
            let to = range.end.min(end) - start;
            for slot in &mut styles[from..to] {
                *slot = style;
            }
        }
        styles
    }

    fn ensure_cursor_visible(&mut self, area: Rect) {
        let rows = usize::from(area.height);
        if rows > 0 {
            let row = self.buffer.cursor_point().row;
            if row < self.viewport_top {
                self.viewport_top = row;
            } else if row >= self.viewport_top + rows {
                self.viewport_top = row + 1 - rows;
            }
        }
        let cols = usize::from(area.width);
        if cols > 0 {
            let col = self.cursor_display_column();
            if col < self.viewport_left {
                self.viewport_left = col;
            } else if col >= self.viewport_left + cols {
                self.viewport_left = col + 1 - cols;
            }
        }
    }

    fn draw_cursor(&self, surface: &mut Surface, theme: &Theme, area: Rect) {
        let row = self.buffer.cursor_point().row;
        if row < self.viewport_top {
            return;
        }
        let screen_row = row - self.viewport_top;
        let column = self.cursor_display_column();
        if column < self.viewport_left {
            return; // cursor scrolled off the left edge
        }
        let screen_col = column - self.viewport_left;
        let (Ok(cx), Ok(cy)) = (u16::try_from(screen_col), u16::try_from(screen_row)) else {
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

    /// The cursor's display column: the sum of glyph widths before it on its line, so the cursor
    /// lands under the right cell when the line contains double-width glyphs.
    fn cursor_display_column(&self) -> usize {
        let point = self.buffer.cursor_point();
        let chars_before = self.buffer.cursor_column();
        self.buffer
            .rope()
            .line(point.row)
            .chars()
            .take(chars_before)
            .map(|ch| usize::from(char_width(ch)))
            .sum()
    }

    /// The status-line text: file name, dirty marker, cursor position, and last status message.
    ///
    /// The host composes this into its status bar (the standalone [`Editor::render`] draws it on
    /// the bottom row; the `mj` app folds it into a global status bar alongside a focus hint).
    #[must_use]
    pub fn status_line(&self) -> String {
        let point = self.buffer.cursor_point();
        let dirty = if self.buffer.is_dirty() { " *" } else { "" };
        // A diagnostic under the cursor is shown inline (its first line, prefixed with a marker).
        let diagnostic = self
            .cursor_diagnostic()
            .map_or_else(String::new, |diagnostic| {
                format!(
                    "   ⚠ {}",
                    diagnostic.message.lines().next().unwrap_or_default()
                )
            });
        format!(
            " {}{dirty}   Ln {}, Col {}   {}{diagnostic}",
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
        editor.flush_highlights(); // highlighting is asynchronous; wait for the first result
        editor.render(&mut surface, &theme);

        // `n` of the `fn` keyword (col 1, no cursor) is drawn in the accent color.
        let cell = surface.cell(1, 0).unwrap();
        assert_eq!(cell.symbol, 'n');
        assert_eq!(cell.style.fg, theme.accent);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&journal);
    }

    #[test]
    fn renders_wide_glyphs_across_two_columns() {
        // A double-width glyph takes two cells, so following text stays aligned.
        let theme = Theme::steelbore();
        let mut editor = Editor::with_buffer(Buffer::from_text("a世b"));
        let mut surface = Surface::new(20, 2, theme.base_style());
        editor.render(&mut surface, &theme);

        assert_eq!(surface.cell(0, 0).unwrap().symbol, 'a');
        assert_eq!(surface.cell(1, 0).unwrap().symbol, '世');
        // Column 2 is the wide glyph's continuation; `b` lands at column 3, not 2.
        assert_eq!(surface.cell(3, 0).unwrap().symbol, 'b');
    }

    #[test]
    fn long_line_scrolls_horizontally_to_follow_the_cursor() {
        let theme = Theme::steelbore();
        // 16 single-width glyphs in an 8-column viewport.
        let mut editor = Editor::with_buffer(Buffer::from_text("0123456789ABCDEF"));
        editor.handle_key(KeyPress::key(KeyCode::End)); // cursor to display column 16
        let mut surface = Surface::new(8, 2, theme.base_style());
        editor.render(&mut surface, &theme);

        // viewport_left = 16 + 1 - 8 = 9, so columns 9..16 ("9ABCDEF") are shown.
        assert_eq!(surface.cell(0, 0).unwrap().symbol, '9');
        assert_eq!(surface.cell(6, 0).unwrap().symbol, 'F');
        // The cursor (display column 16) lands at the last screen column.
        assert!(
            surface.cell(7, 0).unwrap().style.attrs.reverse,
            "cursor at the right edge"
        );
    }

    #[test]
    fn cursor_lands_after_a_wide_glyph() {
        let theme = Theme::steelbore();
        let mut editor = Editor::with_buffer(Buffer::from_text("世x"));
        editor.handle_key(KeyPress::key(KeyCode::Right)); // char col 0 -> 1 (onto `x`)
        let mut surface = Surface::new(20, 2, theme.base_style());
        editor.render(&mut surface, &theme);

        // `世` is two columns wide, so the cursor sits at display column 2, on `x`.
        let cell = surface.cell(2, 0).unwrap();
        assert_eq!(cell.symbol, 'x');
        assert!(
            cell.style.attrs.reverse,
            "cursor should highlight `x` at column 2"
        );
    }

    #[test]
    fn kill_line_cuts_to_line_end_then_joins() {
        let mut editor = Editor::with_buffer(Buffer::from_text("hello\nworld"));
        // Cursor at the start of line 0: `kill-line` removes "hello" onto the clipboard.
        editor.execute("kill-line");
        assert_eq!(editor.buffer().text(), "\nworld");
        assert_eq!(editor.clipboard(), "hello");
        // Cursor now sits at the empty line end: a second kill removes the line break (join).
        editor.execute("kill-line");
        assert_eq!(editor.buffer().text(), "world");
    }

    #[test]
    fn every_documented_command_is_executable() {
        // The catalog↔executor half of the profile guard: no command Oracle documents may fall
        // through `Editor::execute` to the "unbound command" arm.
        for name in oracle::command_names() {
            let mut editor = Editor::with_buffer(Buffer::from_text("alpha\nbeta"));
            editor.execute(name);
            assert!(
                !editor.status().starts_with("unbound command"),
                "documented command `{name}` is not handled by Editor::execute"
            );
        }
    }

    #[test]
    fn vim_normal_mode_swallows_printable_keys_until_insert() {
        use super::EditMode;
        let mut editor = Editor::with_buffer(Buffer::from_text("abc"));
        editor.enable_vim();
        assert_eq!(editor.mode(), EditMode::Normal);
        // A printable key with no Normal-mode binding is swallowed, not inserted.
        editor.handle_key(KeyPress::char('z'));
        assert_eq!(editor.buffer().text(), "abc");
        // `i` enters Insert mode; now printable keys insert.
        editor.handle_key(KeyPress::char('i'));
        assert_eq!(editor.mode(), EditMode::Insert);
        editor.handle_key(KeyPress::char('Z'));
        assert_eq!(editor.buffer().text(), "Zabc");
        // `Esc` returns to Normal; keys are swallowed again.
        editor.handle_key(KeyPress::key(KeyCode::Escape));
        assert_eq!(editor.mode(), EditMode::Normal);
        editor.handle_key(KeyPress::char('q'));
        assert_eq!(editor.buffer().text(), "Zabc");
    }

    #[test]
    fn vim_normal_motion_moves_the_cursor_without_inserting() {
        let mut editor = Editor::with_buffer(Buffer::from_text("abc"));
        editor.enable_vim();
        editor.handle_key(KeyPress::char('l')); // move-right
        editor.handle_key(KeyPress::char('l'));
        assert_eq!(editor.buffer().cursor(), 2);
        assert_eq!(editor.buffer().text(), "abc");
    }

    #[test]
    fn vim_visual_mode_selects_then_yanks() {
        use super::EditMode;
        let mut editor = Editor::with_buffer(Buffer::from_text("abc"));
        editor.enable_vim();
        editor.handle_key(KeyPress::char('v')); // enter Visual mode
        assert_eq!(editor.mode(), EditMode::Visual);
        editor.handle_key(KeyPress::char('l')); // select-right
        editor.handle_key(KeyPress::char('l'));
        editor.handle_key(KeyPress::char('y')); // copy the selection
        assert_eq!(editor.clipboard(), "ab");
        assert_eq!(editor.buffer().text(), "abc");
    }

    #[test]
    fn set_profile_round_trips_modality() {
        use super::EditMode;
        use keymaker::Profile;
        let mut editor = Editor::with_buffer(Buffer::from_text(""));
        editor.set_profile(Profile::Vim);
        assert_eq!(editor.mode(), EditMode::Normal);
        editor.handle_key(KeyPress::char('z')); // Normal mode: swallowed
        assert_eq!(editor.buffer().text(), "");
        editor.set_profile(Profile::Cua); // back to a non-modal profile
        assert_eq!(editor.mode(), EditMode::Insert);
        editor.handle_key(KeyPress::char('z')); // Insert mode: inserts
        assert_eq!(editor.buffer().text(), "z");
    }

    #[test]
    fn profile_switch_under_load_loses_no_keystrokes() {
        // The §8 exit criterion: a live profile switch mid-stream drops nothing. Dispatch is
        // synchronous, so every text-producing key before and after the switch takes effect.
        let mut editor = Editor::with_buffer(Buffer::from_text(""));
        editor.handle_key(KeyPress::char('a')); // CUA (insert)
        editor.handle_key(KeyPress::char('b'));
        editor.execute("profile-emacs"); // live switch between keystrokes
        editor.handle_key(KeyPress::char('c')); // Emacs is also insert-mode
        editor.handle_key(KeyPress::char('d'));
        assert_eq!(editor.buffer().text(), "abcd");
    }

    #[test]
    fn spacemacs_leader_surfaces_which_key_then_runs_a_command() {
        use keymaker::Profile;
        let mut editor = Editor::with_buffer(Buffer::from_text(""));
        editor.set_profile(Profile::Spacemacs);
        assert!(editor.which_key().is_empty()); // nothing pending yet
        editor.handle_key(KeyPress::char(' ')); // SPC starts the leader
        let hints = editor.which_key();
        assert!(
            !hints.is_empty(),
            "SPC should surface which-key continuations"
        );
        // SPC f s runs `save` (no file -> it fails, but the leader sequence inserts nothing).
        editor.handle_key(KeyPress::char('f'));
        editor.handle_key(KeyPress::char('s'));
        assert_eq!(editor.buffer().text(), "");
        assert!(editor.which_key().is_empty()); // sequence resolved -> hint gone
    }

    #[test]
    fn spacemacs_space_inserts_in_insert_mode() {
        // SPC is the leader only in Normal mode; in Insert mode it must insert a space.
        use keymaker::Profile;
        let mut editor = Editor::with_buffer(Buffer::from_text(""));
        editor.set_profile(Profile::Spacemacs);
        editor.handle_key(KeyPress::char('i')); // enter Insert
        editor.handle_key(KeyPress::char('a'));
        editor.handle_key(KeyPress::char(' '));
        editor.handle_key(KeyPress::char('b'));
        assert_eq!(editor.buffer().text(), "a b");
    }

    #[test]
    fn diagnostics_underline_their_span_in_the_severity_color() {
        use crate::diagnostic::{Diagnostic, Severity};
        let theme = Theme::steelbore();
        let mut editor = Editor::with_buffer(Buffer::from_text("let x = oops;"));
        // Underline `oops` (bytes 8..12) as an error.
        editor.set_diagnostics(vec![Diagnostic::new(
            8..12,
            Severity::Error,
            "cannot find value `oops`",
        )]);
        let mut surface = Surface::new(40, 2, theme.base_style());
        editor.render(&mut surface, &theme);

        let inside = surface.cell(8, 0).unwrap(); // 'o' of `oops`
        assert!(inside.style.attrs.underline, "the span is underlined");
        assert_eq!(inside.style.fg, theme.error, "in the error color");
        // Text outside the diagnostic is untouched.
        assert!(!surface.cell(0, 0).unwrap().style.attrs.underline);
    }

    #[test]
    fn cursor_diagnostic_appears_in_the_status_line() {
        use crate::diagnostic::{Diagnostic, Severity};
        let mut editor = Editor::with_buffer(Buffer::from_text("let x = oops;"));
        editor.set_diagnostics(vec![Diagnostic::new(
            8..12,
            Severity::Error,
            "cannot find value `oops`",
        )]);
        for _ in 0..9 {
            editor.handle_key(KeyPress::key(KeyCode::Right)); // move into the span (byte 9)
        }
        assert!(editor.cursor_diagnostic().is_some());
        assert!(editor.status_line().contains("cannot find value"));

        editor.handle_key(KeyPress::key(KeyCode::Home)); // move out of the span
        assert!(editor.cursor_diagnostic().is_none());
    }
}
