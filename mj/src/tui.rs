// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The interactive terminal front-end: a crossterm raw-mode loop driving the editor and an
//! optional integrated terminal panel.
//!
//! This is the only place that touches the real terminal. It enables raw mode + the alternate
//! screen (restored on drop, even on panic), reads crossterm events, and routes them through
//! an [`App`] that holds the [`Editor`] and a lazily-spawned [`PtyTerminal`]. The screen is laid
//! out per `UI.md`: the editor area on top, a labelled **terminal panel** docked along the
//! bottom (a tabbed divider in Steel Blue), and a one-row global status bar beneath it. `F12`
//! spawns/toggles focus between the editor and the terminal panel; while the panel is focused,
//! keys are encoded to bytes and written to the PTY. Each frame is presented through a Penumbra
//! [`Screen`]. The editor model itself is backend-agnostic and tested headless.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as TermKey, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use keymaker::{KeyCode, KeyPress, Mods, Profile};
use majestic_core::{
    Action, Completion, Editor, FileTree, Finder, HelpOverlay, Hover, InfoReader, ProfileSelector,
    References, Session, Workspace,
};
use majestic_lsp::{LspManager, LspOutcome};
use majestic_term::PtyTerminal;
use penumbra::{Buffer, Rect, Screen, Style, Theme};

/// The `F12` key spawns/toggles the integrated terminal (reassignable once the Architect lands at M3).
const TERMINAL_TOGGLE: KeyCode = KeyCode::Function(12);

/// Default height of the bottom terminal panel in rows (UI.md §5: 8–15, resizable later).
const PANEL_ROWS: u16 = 10;

/// Editor rows kept visible above the panel; below this total the panel is hidden.
const MIN_EDITOR_ROWS: u16 = 3;

/// Width of the explorer sidebar in columns (UI.md §2: 20–35, resizable later).
const SIDEBAR_COLS: u16 = 28;

/// Editor columns kept usable beside the sidebar; below this the sidebar is not drawn.
const MIN_MAIN_COLS: u16 = 24;

/// Restores the terminal (cooked mode, main screen, visible cursor) when dropped.
pub(crate) struct TerminalGuard;

impl TerminalGuard {
    pub(crate) fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            cursor::Hide
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            cursor::Show,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// Which surface currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Explorer,
    Terminal,
}

/// The `Ctrl+B` key toggles the explorer sidebar (VS Code convention).
const SIDEBAR_TOGGLE: KeyPress = KeyPress::ctrl('b');

/// The `Ctrl+P` key opens the fuzzy file finder (the command palette is `Ctrl+Shift+P`).
const FILE_FINDER: KeyPress = KeyPress::ctrl('p');

/// The `F1` key opens (and closes) the Oracle key-bindings help overlay.
const HELP_KEY: KeyPress = KeyPress::key(KeyCode::Function(1));

/// The `Ctrl+Space` key requests LSP completion at the cursor (the editor convention).
const COMPLETION_KEY: KeyPress = KeyPress::ctrl(' ');

/// The `F2` key requests LSP hover documentation at the cursor (the keyboard counterpart to mouse
/// hover; an F-key, like F1/F12, so it is safe to capture globally without shadowing an editing key).
const HOVER_KEY: KeyPress = KeyPress::key(KeyCode::Function(2));

/// The `F12` key requests LSP goto-definition at the cursor (the universal editor convention).
const GOTO_DEF_KEY: KeyPress = KeyPress::key(KeyCode::Function(12));

/// The `Shift+F12` key requests LSP find-references at the cursor (the universal "Find All
/// References" shortcut, the companion to `F12` goto-definition). Matched via [`is_references_key`]
/// (tolerant of how terminals report the chord), and dispatched before the modifier-agnostic
/// terminal toggle so `Shift+F12` opens the references popup rather than toggling the panel.
const REFERENCES_KEY: KeyPress = KeyPress::new(Mods::SHIFT, KeyCode::Function(12));

/// The running application: the editor workspace, an optional explorer sidebar, and an optional
/// integrated terminal.
struct App {
    workspace: Workspace,
    explorer: Option<FileTree>,
    sidebar_visible: bool,
    terminal: Option<PtyTerminal>,
    finder: Option<Finder>,
    help: Option<HelpOverlay>,
    info: Option<InfoReader>,
    /// The first-run profile picker; `Some` until the user chooses (modal while open).
    selector: Option<ProfileSelector>,
    /// The LSP completion popup; `Some` while candidates are shown over the editor.
    completion: Option<Completion>,
    /// The byte offset where the in-progress identifier (the prefix being completed) starts; an
    /// accepted candidate replaces `completion_anchor..cursor`.
    completion_anchor: usize,
    /// The LSP hover popup; `Some` while documentation is shown over the editor.
    hover: Option<Hover>,
    /// The LSP find-references popup; `Some` while a symbol's use sites are shown over the editor.
    references: Option<References>,
    /// Language servers + document sync (diagnostics). Servers start lazily on first matching file.
    lsp: LspManager,
    /// The buffer revision last sent to a language server, keyed by path (so an unchanged buffer
    /// is not re-synced each frame).
    lsp_synced: HashMap<PathBuf, u64>,
    /// The path + buffer revision of an in-flight format request, recorded when `Shift+Alt+F` is
    /// pressed. A returned reformat is applied only while the buffer is still on this revision, so an
    /// edit made while the request was in flight is never clobbered by stale output.
    format_request: Option<(PathBuf, u64)>,
    focus: Focus,
}

impl App {
    fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            explorer: None,
            sidebar_visible: false,
            terminal: None,
            finder: None,
            help: None,
            info: None,
            selector: None,
            completion: None,
            completion_anchor: 0,
            hover: None,
            references: None,
            lsp: LspManager::with_defaults(),
            lsp_synced: HashMap::new(),
            format_request: None,
            focus: Focus::Editor,
        }
    }

    /// Reconciles the focused buffer with its language server and applies any published
    /// diagnostics. Cheap each frame: it only sends `didOpen`/`didChange` when the buffer's
    /// revision changed, and `poll` is non-blocking (servers start on a background thread).
    fn sync_lsp(&mut self) {
        let active = {
            let editor = self.workspace.active();
            editor
                .buffer()
                .path()
                .map(|path| (path, editor.buffer().revision()))
        };
        if let Some((path, revision)) = active {
            if self.lsp.handles(&path) && self.lsp_synced.get(&path).copied() != Some(revision) {
                let first = !self.lsp_synced.contains_key(&path);
                let text = self.workspace.active().buffer().text();
                let result = if first {
                    self.lsp.open(&path, &text)
                } else {
                    self.lsp.change(&path, &text)
                };
                if result.is_ok() {
                    self.lsp_synced.insert(path, revision);
                }
            }
        }
        for (path, diagnostics) in self.lsp.poll() {
            self.workspace.apply_diagnostics(&path, &diagnostics);
        }
        // Interactive-request results (completion candidates, hover docs, references) arrive
        // asynchronously; open the matching popup when a result is for the buffer that still has
        // focus. The three cursor popups are mutually exclusive, so opening one closes the others.
        for outcome in self.lsp.poll_outcomes() {
            let active_path = self.workspace.active().buffer().path();
            let focused_match =
                self.focus == Focus::Editor && active_path.as_deref().is_some_and(|active| {
                    matches!(&outcome, LspOutcome::Completion { path, .. } | LspOutcome::Hover { path, .. } | LspOutcome::GotoDefinition { path, .. } | LspOutcome::References { path, .. } | LspOutcome::Formatting { path, .. } if path == active)
                });
            if !focused_match {
                continue;
            }
            match outcome {
                LspOutcome::Completion { items, .. } => {
                    if !items.is_empty() {
                        self.completion = Some(Completion::new(items));
                        self.hover = None;
                        self.references = None;
                    }
                }
                LspOutcome::Hover { text, .. } => {
                    if let Some(hover) =
                        text.map(|text| Hover::new(&text)).filter(|h| !h.is_empty())
                    {
                        self.hover = Some(hover);
                        self.completion = None;
                        self.references = None;
                    }
                }
                LspOutcome::GotoDefinition { target, .. } => {
                    if let Some((target_path, position)) = target {
                        // Reveal the destination (reusing an open editor when possible), then land
                        // the cursor on the target position converted against that file's text.
                        if self.workspace.reveal_path(&target_path).is_ok() {
                            let text = self.workspace.active().buffer().text();
                            let byte = majestic_lsp::position_to_byte(
                                &text,
                                position.line,
                                position.character,
                            );
                            self.workspace.set_active_cursor(byte);
                            self.completion = None;
                            self.hover = None;
                            self.references = None;
                        }
                    }
                }
                LspOutcome::References { references, .. } => {
                    let references = References::new(references);
                    if !references.is_empty() {
                        self.references = Some(references);
                        self.completion = None;
                        self.hover = None;
                    }
                }
                LspOutcome::Formatting { path, formatted } => {
                    self.apply_formatting(&path, formatted);
                }
            }
        }
    }

    /// `true` while a live shell exists (so the loop polls quickly for its output).
    fn terminal_running(&self) -> bool {
        self.terminal.as_ref().is_some_and(PtyTerminal::is_running)
    }

    /// Closes the panel and returns to the editor once the shell has exited.
    fn reap_dead_terminal(&mut self) {
        if self
            .terminal
            .as_ref()
            .is_some_and(|term| !term.is_running())
        {
            self.terminal = None;
            self.focus = Focus::Editor;
        }
    }

    fn should_quit(&self) -> bool {
        self.workspace.should_quit()
    }

    /// The editing workspace (so the daemon can snapshot it into a session on detach).
    pub(crate) fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Draws the sidebar, the editor area, the bottom terminal panel (when present), and the
    /// status bar — the full UI.md layout.
    fn render(&mut self, surface: &mut Buffer, theme: &Theme) {
        self.sync_lsp(); // reconcile document sync + apply diagnostics before drawing
        surface.clear(theme.base_style());
        let (body, status) = surface.area().split_bottom(1);
        let main = self.render_sidebar(surface, body, theme);

        // The region the editor was actually drawn into (none while the Info reader is showing),
        // used to anchor the completion popup at the cursor.
        let mut editor_rect: Option<Rect> = None;

        if let Some(info) = self.info.as_mut() {
            // The Info reader takes over the editor region (the sidebar + status bar remain).
            info.render(surface, main, theme);
        } else if self.terminal.is_some() && main.height > MIN_EDITOR_ROWS + 1 {
            let panel_rows = PANEL_ROWS.min(main.height - (MIN_EDITOR_ROWS + 1));
            let (editor_area, block) = main.split_bottom(panel_rows + 1);
            let (divider, panel_area) = block.split_top(1);

            // Keep the shell sized to its panel so its grid matches what we draw.
            if let Some(term) = self.terminal.as_mut() {
                if term.columns() != usize::from(panel_area.width)
                    || term.screen_lines() != usize::from(panel_area.height)
                {
                    term.resize(
                        usize::from(panel_area.width),
                        usize::from(panel_area.height),
                    );
                }
            }

            self.workspace
                .render(surface, editor_area, theme, self.focus == Focus::Editor);
            editor_rect = Some(editor_area);
            draw_panel_tab(surface, divider, theme, self.focus == Focus::Terminal);
            if let Some(term) = self.terminal.as_ref() {
                term.render_in(surface, panel_area, theme, self.focus == Focus::Terminal);
            }
        } else {
            self.workspace
                .render(surface, main, theme, self.focus == Focus::Editor);
            editor_rect = Some(main);
        }

        self.draw_status_bar(surface, status.y, theme);

        // which-key hint: while a prefix is in progress in the editor (Spacemacs SPC, Emacs C-x)
        // and no modal is open, list the keys that may come next over the editor area.
        if self.focus == Focus::Editor
            && self.finder.is_none()
            && self.help.is_none()
            && self.info.is_none()
            && self.selector.is_none()
        {
            if let Some(which_key) = self.workspace.which_key() {
                which_key.render(surface, main, theme);
            }
        }

        // Modal overlays are drawn last, over everything else.
        let area = surface.area();
        if let Some(finder) = self.finder.as_ref() {
            finder.render(surface, area, theme);
        }
        if let Some(help) = self.help.as_ref() {
            help.render(surface, area, theme);
        }
        if let Some(selector) = self.selector.as_ref() {
            selector.render(surface, area, theme);
        }

        // The completion, hover, and references popups are anchored at the cursor within the editor
        // region, above all else (they are mutually exclusive, so at most one shows).
        if let (Some(completion), Some(rect)) = (self.completion.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                completion.render(surface, rect, cursor, theme);
            }
        }
        if let (Some(hover), Some(rect)) = (self.hover.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                hover.render(surface, rect, cursor, theme);
            }
        }
        if let (Some(references), Some(rect)) = (self.references.as_ref(), editor_rect) {
            if let Some(cursor) = self.workspace.active_cursor_screen(rect) {
                references.render(surface, rect, cursor, theme);
            }
        }
    }

    /// Draws the explorer sidebar on the left (when shown and wide enough), returning the
    /// remaining region for the editor/terminal stack.
    fn render_sidebar(&mut self, surface: &mut Buffer, body: Rect, theme: &Theme) -> Rect {
        if !self.sidebar_visible {
            return body;
        }
        let focused = self.focus == Focus::Explorer;
        let Some(explorer) = self.explorer.as_mut() else {
            return body;
        };
        let sidebar_cols = SIDEBAR_COLS.min(body.width.saturating_sub(MIN_MAIN_COLS + 1));
        if sidebar_cols == 0 {
            return body; // too narrow to show the sidebar; keep the full main area
        }
        let (sidebar, rest) = body.split_left(sidebar_cols);
        let (divider, main) = rest.split_left(1);
        explorer.render(surface, sidebar, theme, focused);
        let rule = Style::new(theme.accent, theme.background); // Steel Blue
        for y in divider.y..divider.bottom() {
            surface.set_char(divider.x, y, '│', rule);
        }
        main
    }

    /// Draws the global status bar: the editor's status line plus a focus/terminal hint.
    fn draw_status_bar(&self, surface: &mut Buffer, row: u16, theme: &Theme) {
        let style = Style::new(theme.background, theme.accent);
        for x in 0..surface.width() {
            surface.set_char(x, row, ' ', style);
        }
        surface.set_str(0, row, &self.workspace.status_line(), style);

        let hint = if self.terminal.is_some() {
            if self.focus == Focus::Terminal {
                "[F1 help · F12 ⇄ EDITOR · Ctrl+B files]"
            } else {
                "[F1 help · F12 ⇄ TERMINAL · Ctrl+B files]"
            }
        } else {
            "[F1 help · F12 terminal · Ctrl+B files]"
        };
        if let Ok(len) = u16::try_from(hint.chars().count()) {
            if len < surface.width() {
                surface.set_str(surface.width() - len - 1, row, hint, style);
            }
        }
    }

    fn handle_key(&mut self, key: KeyPress, columns: u16, lines: u16) -> io::Result<()> {
        // The first-run selector is modal and outranks everything: it captures every key until
        // the user has chosen a keybinding profile.
        if self.selector.is_some() {
            self.selector_key(key);
            return Ok(());
        }
        // The help overlay and the fuzzy finder are modal: while open they capture every key.
        if self.help.is_some() {
            self.help_key(key);
            return Ok(());
        }
        if self.finder.is_some() {
            self.finder_key(key);
            return Ok(());
        }
        if self.info.is_some() {
            self.info_key(key);
            return Ok(());
        }
        // The completion and hover popups are light modals: each captures its navigation/accept
        // keys while open, and any other key dismisses it and falls through to normal editing.
        if self.completion.is_some() && self.completion_popup_key(key) {
            return Ok(());
        }
        if self.hover.is_some() && self.hover_popup_key(key) {
            return Ok(());
        }
        if self.references.is_some() && self.references_popup_key(key) {
            return Ok(());
        }
        if key == HELP_KEY {
            self.help = Some(HelpOverlay::new(
                "Key Bindings (Esc to close)",
                &oracle::describe_bindings(&keymaker::cua()),
            ));
            return Ok(());
        }
        if is_command_palette(key) {
            self.finder = Some(Finder::commands(&oracle::command_names()));
            return Ok(());
        }
        if key == FILE_FINDER {
            let root = self.project_root();
            self.finder = Some(Finder::files(&root));
            return Ok(());
        }
        if key == COMPLETION_KEY {
            self.trigger_completion();
            return Ok(());
        }
        if key == HOVER_KEY {
            self.trigger_hover();
            return Ok(());
        }
        if key == GOTO_DEF_KEY {
            self.trigger_goto_definition();
            return Ok(());
        }
        if is_references_key(key) {
            self.trigger_references();
            return Ok(());
        }
        if is_format_key(key) {
            self.trigger_formatting();
            return Ok(());
        }
        if key.code == TERMINAL_TOGGLE {
            self.toggle_terminal(columns, lines);
            return Ok(());
        }
        if key == SIDEBAR_TOGGLE {
            self.toggle_sidebar();
            return Ok(());
        }
        match self.focus {
            Focus::Terminal => {
                if let Some(term) = self.terminal.as_mut() {
                    if let Some(bytes) = encode_key(key) {
                        term.write_input(&bytes)?;
                    }
                }
            }
            Focus::Explorer => self.explorer_key(key),
            Focus::Editor => self.workspace.handle_key(key),
        }
        Ok(())
    }

    /// Routes a key to the open completion popup. Returns `true` when the key was consumed (the
    /// caller returns), `false` when the key dismissed the popup and should fall through to editing.
    fn completion_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(completion) = self.completion.as_mut() {
                    completion.select_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(completion) = self.completion.as_mut() {
                    completion.select_down();
                }
                true
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.accept_completion();
                true
            }
            KeyCode::Escape => {
                self.completion = None;
                true
            }
            _ => {
                self.completion = None;
                false
            }
        }
    }

    /// Routes a key to the open hover popup: ↑/↓ scroll it, `Esc` closes it, and any other key
    /// dismisses it and falls through (so pressing `F2` again re-requests hover at the new cursor).
    /// Returns `true` when the key was consumed, `false` when it should fall through to editing.
    fn hover_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(hover) = self.hover.as_mut() {
                    hover.scroll_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(hover) = self.hover.as_mut() {
                    hover.scroll_down();
                }
                true
            }
            KeyCode::Escape => {
                self.hover = None;
                true
            }
            _ => {
                self.hover = None;
                false
            }
        }
    }

    /// Routes a key to the open references popup: ↑/↓ move the selection, `Enter` jumps to the
    /// selected use site, `Esc` closes it, and any other key dismisses it and falls through to
    /// editing. Returns `true` when the key was consumed, `false` when it should fall through.
    fn references_popup_key(&mut self, key: KeyPress) -> bool {
        match key.code {
            KeyCode::Up => {
                if let Some(references) = self.references.as_mut() {
                    references.select_up();
                }
                true
            }
            KeyCode::Down => {
                if let Some(references) = self.references.as_mut() {
                    references.select_down();
                }
                true
            }
            KeyCode::Enter => {
                self.jump_to_selected_reference();
                true
            }
            KeyCode::Escape => {
                self.references = None;
                true
            }
            _ => {
                self.references = None;
                false
            }
        }
    }

    /// Jumps to the selected reference and closes the popup: reveals its file (reusing an open editor
    /// when possible) and lands the cursor on the use site, converting its LSP position against that
    /// file's current text — the same jump path as goto-definition.
    fn jump_to_selected_reference(&mut self) {
        let Some(references) = self.references.take() else {
            return;
        };
        let Some(reference) = references.selected() else {
            return;
        };
        let (path, line, character) = (reference.path.clone(), reference.line, reference.character);
        if self.workspace.reveal_path(&path).is_ok() {
            let text = self.workspace.active().buffer().text();
            let byte = majestic_lsp::position_to_byte(&text, line, character);
            self.workspace.set_active_cursor(byte);
        }
    }

    /// Requests LSP completion at the cursor: records where the in-progress identifier starts (so an
    /// accepted candidate replaces it, not the whole word), then asks the manager to fetch
    /// candidates off-thread. A no-op unless the editor is focused and a server handles the buffer;
    /// the popup opens later, in `sync_lsp`, once the result arrives.
    fn trigger_completion(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        let text = editor.buffer().text();
        self.completion_anchor = identifier_start(&text, cursor);
        self.lsp.request_completion(&path, cursor);
    }

    /// Requests LSP hover documentation at the cursor, off-thread; the popup opens later, in
    /// `sync_lsp`, once the reply arrives. A no-op unless the editor is focused and a server handles
    /// the buffer.
    fn trigger_hover(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_hover(&path, cursor);
    }

    /// Requests LSP goto-definition at the cursor, off-thread; the jump happens later, in
    /// `sync_lsp`, once the reply arrives. A no-op unless the editor is focused and a server handles
    /// the buffer.
    fn trigger_goto_definition(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_goto_definition(&path, cursor);
    }

    /// Requests LSP find-references at the cursor, off-thread; the popup opens later, in `sync_lsp`,
    /// once the reply arrives. A no-op unless the editor is focused and a server handles the buffer.
    fn trigger_references(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        let cursor = editor.buffer().cursor();
        self.lsp.request_references(&path, cursor);
    }

    /// Requests a whole-document LSP reformat, off-thread; the result is applied later, in
    /// `sync_lsp`, once the reply arrives — but only if the buffer is still on the revision recorded
    /// here, so an edit made while formatting was in flight is never clobbered by stale output. A
    /// no-op unless the editor is focused and a server handles the buffer.
    fn trigger_formatting(&mut self) {
        if self.focus != Focus::Editor {
            return;
        }
        let editor = self.workspace.active();
        let Some(path) = editor.buffer().path() else {
            return;
        };
        if !self.lsp.handles(&path) {
            return;
        }
        self.format_request = Some((path.clone(), editor.buffer().revision()));
        self.lsp.request_formatting(&path);
    }

    /// Applies a completed reformat to the focused buffer — but only if `formatted` is `Some` and the
    /// buffer is still the one, and on the revision, that was formatted (otherwise the result is
    /// stale and dropped, never overwriting newer edits). The whole document is replaced as one
    /// undoable edit and the cursor is re-seated at its old byte offset (clamped to the new text),
    /// since formatting usually only adjusts surrounding whitespace.
    fn apply_formatting(&mut self, path: &Path, formatted: Option<String>) {
        let Some((requested_path, revision)) = self.format_request.take() else {
            return;
        };
        let Some(formatted) = formatted else {
            return;
        };
        let (current_path, current_revision, cursor, len) = {
            let buffer = self.workspace.active().buffer();
            (
                buffer.path(),
                buffer.revision(),
                buffer.cursor(),
                buffer.text().len(),
            )
        };
        if requested_path.as_path() != path
            || current_path.as_deref() != Some(path)
            || current_revision != revision
        {
            return;
        }
        self.workspace.replace_active(0..len, &formatted);
        self.workspace
            .set_active_cursor(cursor.min(formatted.len()));
        self.completion = None;
        self.hover = None;
    }

    /// Inserts the selected candidate over the typed identifier prefix and closes the popup.
    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(item) = completion.selected() else {
            return;
        };
        let cursor = self.workspace.active().buffer().cursor();
        let start = self.completion_anchor.min(cursor);
        self.workspace
            .replace_active(start..cursor, &item.insert_text);
    }

    /// Routes a key to the explorer: arrow navigation, `Enter` to open/expand, `Esc` to leave.
    fn explorer_key(&mut self, key: KeyPress) {
        if key.code == KeyCode::Escape {
            self.focus = Focus::Editor;
            return;
        }
        let opened = if let Some(explorer) = self.explorer.as_mut() {
            match key.code {
                KeyCode::Up => {
                    explorer.select_up();
                    None
                }
                KeyCode::Down => {
                    explorer.select_down();
                    None
                }
                KeyCode::Enter => explorer.activate(),
                _ => None,
            }
        } else {
            self.focus = Focus::Editor;
            None
        };
        if let Some(path) = opened {
            self.open_path(&path);
        }
    }

    /// Opens `path` as a new buffer in the workspace and moves focus to the editor.
    fn open_path(&mut self, path: &Path) {
        // GNU Info documents open in the built-in reader (M1 §5.7) rather than the text editor.
        if path
            .extension()
            .is_some_and(|extension| extension == "info")
        {
            if let Ok(reader) = InfoReader::open(path) {
                self.info = Some(reader);
                self.focus = Focus::Editor;
                return;
            }
        }
        if let Ok(buffer) = majestic_core::Buffer::open(path) {
            self.workspace.open(Editor::with_buffer(buffer));
            self.focus = Focus::Editor;
        }
        // A failed open keeps focus on the explorer; surfaced errors arrive with the minibuffer.
    }

    /// `Ctrl+B`: open+focus the sidebar, focus it if already shown, or hide it when focused.
    fn toggle_sidebar(&mut self) {
        if !self.sidebar_visible {
            if let Some(explorer) = self.explorer.as_mut() {
                // Re-opening: rescan the tree and git status so decorations are current.
                explorer.refresh();
            } else {
                let root =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                self.explorer = Some(FileTree::new(root));
            }
            self.sidebar_visible = true;
            self.focus = Focus::Explorer;
        } else if self.focus == Focus::Explorer {
            self.sidebar_visible = false;
            self.focus = Focus::Editor;
        } else {
            self.focus = Focus::Explorer;
        }
    }

    /// The directory the fuzzy file finder searches: the explorer root, else the working dir.
    fn project_root(&self) -> PathBuf {
        self.explorer.as_ref().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            |explorer| explorer.root().to_path_buf(),
        )
    }

    /// Routes a key to the open finder modal: type to filter, arrows to move, Enter/Esc to
    /// accept/cancel.
    fn finder_key(&mut self, key: KeyPress) {
        match key.code {
            KeyCode::Escape => self.finder = None,
            KeyCode::Enter => self.finder_accept(),
            KeyCode::Up => {
                if let Some(finder) = self.finder.as_mut() {
                    finder.select_up();
                }
            }
            KeyCode::Down => {
                if let Some(finder) = self.finder.as_mut() {
                    finder.select_down();
                }
            }
            KeyCode::Backspace => {
                if let Some(finder) = self.finder.as_mut() {
                    finder.backspace();
                }
            }
            KeyCode::Char(c)
                if !key.mods.contains(Mods::CTRL)
                    && !key.mods.contains(Mods::ALT)
                    && !key.mods.contains(Mods::SUPER) =>
            {
                if let Some(finder) = self.finder.as_mut() {
                    finder.push(c);
                }
            }
            _ => {}
        }
    }

    /// Performs the selected finder action (open a file / run a command) and closes the modal.
    fn finder_accept(&mut self) {
        let Some(action) = self.finder.as_ref().and_then(Finder::accept).cloned() else {
            self.finder = None;
            return;
        };
        self.finder = None;
        match action {
            Action::OpenFile(path) => self.open_path(&path),
            Action::RunCommand(name) => self.workspace.execute(&name),
        }
    }

    /// Routes a key to the open Info reader: `n`/`p`/`u` navigate, Enter follows the selected
    /// menu entry, `l` goes back, arrows/Page scroll and move the menu selection, `q`/Esc closes.
    fn info_key(&mut self, key: KeyPress) {
        if key.code == KeyCode::Escape || key == KeyPress::char('q') {
            self.info = None;
            return;
        }
        let Some(info) = self.info.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Char('n') => info.next(),
            KeyCode::Char('p') => info.prev(),
            KeyCode::Char('u') => info.up(),
            KeyCode::Char('l') => info.back(),
            KeyCode::Enter => info.enter(),
            KeyCode::Up => info.select_up(),
            KeyCode::Down => info.select_down(),
            KeyCode::PageUp => info.scroll_up(10),
            KeyCode::PageDown | KeyCode::Char(' ') => info.scroll_down(10),
            _ => {}
        }
    }

    /// Routes a key to the first-run selector: a mnemonic letter chooses that profile, `Esc`
    /// accepts the CUA default. The choice is applied live and persisted so the prompt does not
    /// reappear; a persistence failure is non-fatal (it just re-prompts next launch).
    fn selector_key(&mut self, key: KeyPress) {
        let Some(selector) = self.selector.as_ref() else {
            return;
        };
        let chosen = match key.code {
            KeyCode::Escape => Some(Profile::Cua),
            KeyCode::Char(ch) => selector.choose(ch),
            _ => None,
        };
        let Some(profile) = chosen else {
            return; // an unrecognized key keeps the modal open
        };
        self.selector = None;
        self.workspace.set_profile(profile);
        match majestic_config::write_keymap(profile.name()) {
            Ok(path) => self.workspace.set_status(format!(
                "keybindings: {} — saved to {}",
                profile.name(),
                path.display()
            )),
            Err(error) => self.workspace.set_status(format!(
                "keybindings: {} (not saved: {error})",
                profile.name()
            )),
        }
    }

    /// Routes a key to the open help overlay: arrows/Page scroll, Esc or F1 close.
    fn help_key(&mut self, key: KeyPress) {
        if key.code == KeyCode::Escape || key == HELP_KEY {
            self.help = None;
            return;
        }
        if let Some(help) = self.help.as_mut() {
            match key.code {
                KeyCode::Up => help.scroll_up(1),
                KeyCode::Down => help.scroll_down(1),
                KeyCode::PageUp => help.scroll_up(10),
                KeyCode::PageDown => help.scroll_down(10),
                _ => {}
            }
        }
    }

    fn paste(&mut self, text: &str) -> io::Result<()> {
        if self.focus == Focus::Terminal {
            if let Some(term) = self.terminal.as_mut() {
                return term.write_input(text.as_bytes());
            }
        }
        self.workspace.insert_text(text);
        Ok(())
    }

    /// `F12`: spawn the shell on first use (focusing it), otherwise flip focus between the
    /// terminal panel and the editor — the shell session survives toggling.
    fn toggle_terminal(&mut self, columns: u16, lines: u16) {
        if self.terminal_running() {
            self.focus = if self.focus == Focus::Terminal {
                Focus::Editor
            } else {
                Focus::Terminal
            };
            return;
        }
        // No live shell: (re)spawn one and focus the panel. It is resized to the panel on render.
        self.terminal = PtyTerminal::spawn(usize::from(columns), usize::from(lines)).ok();
        self.focus = if self.terminal.is_some() {
            Focus::Terminal
        } else {
            Focus::Editor
        };
    }
}

/// Draws the terminal panel's tabbed divider row: a Steel Blue rule with a `TERMINAL` tab,
/// highlighted (reverse) when the panel holds focus (UI.md §5 bottom-panel tab bar).
fn draw_panel_tab(surface: &mut Buffer, area: Rect, theme: &Theme, focused: bool) {
    if area.is_empty() {
        return;
    }
    let rule = Style::new(theme.accent, theme.background); // Steel Blue box-drawing on Void Navy
    for x in area.x..area.right() {
        surface.set_char(x, area.y, '─', rule);
    }
    let label_style = if focused {
        Style::new(theme.background, theme.accent) // active tab: Void Navy on Steel Blue
    } else {
        rule
    };
    surface.set_str(area.x.saturating_add(1), area.y, " TERMINAL ", label_style);
}

/// A headless editor session the daemon drives over a socket.
///
/// It owns the full [`App`] and renders into an off-screen [`Buffer`] — the exact frame `run` would
/// have drawn locally. The daemon mirrors that frame to every attached client, diffing it against
/// each client's own front buffer, so a late joiner gets a full repaint while the others get only
/// the delta (see [`SessionHost::render_frame`]). The whole editor UI (sidebar, panels, overlays)
/// is reused without touching a real terminal; only input and output go to the wire.
pub(crate) struct SessionHost {
    app: App,
    /// The latest rendered frame, mirrored to every attached client.
    frame: Buffer,
    theme: Theme,
    cols: u16,
    rows: u16,
}

impl SessionHost {
    /// Creates a host over `workspace` at the given client terminal size.
    pub(crate) fn new(workspace: Workspace, cols: u16, rows: u16) -> Self {
        let theme = Theme::steelbore();
        let (cols, rows) = (cols.max(1), rows.max(1));
        let frame = Buffer::new(cols, rows, theme.base_style());
        Self {
            app: App::new(workspace),
            frame,
            theme,
            cols,
            rows,
        }
    }

    /// Resizes the off-screen surface to a (new) mirrored size (the smallest attached terminal).
    ///
    /// A no-op when the size is unchanged, so redundant renegotiation does not resize the frame and
    /// thereby force a needless full repaint on clients whose own size did not change.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.frame.resize(cols, rows, self.theme.base_style());
    }

    /// Feeds one key; returns whether the editor asked to quit.
    ///
    /// # Errors
    /// Propagates an I/O error from the integrated terminal panel.
    pub(crate) fn input(&mut self, key: KeyPress) -> io::Result<bool> {
        self.app.handle_key(key, self.cols, self.rows)?;
        Ok(self.app.should_quit())
    }

    /// Renders the current state into the shared frame and returns it. The daemon diffs this against
    /// each client's front buffer (via [`penumbra::render`]) to produce that client's byte stream —
    /// a full repaint for a freshly attached client (its front buffer is empty), a minimal delta for
    /// the rest.
    pub(crate) fn render_frame(&mut self) -> &Buffer {
        self.app.reap_dead_terminal();
        self.app.render(&mut self.frame, &self.theme);
        &self.frame
    }

    /// Snapshots the current layout/open-files/cursors into a [`Session`] (persisted on detach).
    pub(crate) fn to_session(&self) -> Session {
        self.app.workspace().to_session()
    }

    /// Whether the integrated terminal is streaming output — the daemon ticks faster while it is,
    /// so a shell's output keeps painting for an attached client (matches `run`'s cadence).
    pub(crate) fn terminal_running(&self) -> bool {
        self.app.terminal_running()
    }
}

/// Runs the editor + terminal interactive loop until a quit command is issued.
///
/// When `persist_session` is set, the workspace layout is saved to the session file on exit so a
/// later plain `mj` reopens it (the transient `mj info` view passes `false`).
///
/// # Errors
/// Returns any terminal I/O error from setup, reading events, or rendering.
pub(crate) fn run(
    workspace: Workspace,
    initial_info: Option<PathBuf>,
    first_run: bool,
    persist_session: bool,
) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let theme = Theme::steelbore();
    let (columns, lines) = terminal::size()?;
    let mut screen = Screen::new(columns, lines, theme.base_style());
    let mut out = io::stdout();
    let mut app = App::new(workspace);
    if first_run {
        // No manifest yet: ask the user to pick a keybinding profile before editing.
        app.selector = Some(ProfileSelector::new());
    }
    if let Some(path) = initial_info {
        // A launch-time Info path (an `.info` argument, or `mj info`) opens the reader directly —
        // including the extension-less `dir` directory file.
        if let Ok(reader) = InfoReader::open(&path) {
            app.info = Some(reader);
        }
    }

    loop {
        app.reap_dead_terminal();
        app.render(screen.back_mut(), &theme);
        screen.present(&mut out)?;
        out.flush()?;

        // Poll quickly while a shell streams output (even when the editor is focused); idle
        // longer when only editing.
        let timeout = if app.terminal_running() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(200)
        };
        if event::poll(timeout)? {
            let (columns, lines) = (screen.front().width(), screen.front().height());
            match event::read()? {
                Event::Key(key) => {
                    if let Some(press) = translate(key) {
                        app.handle_key(press, columns, lines)?;
                    }
                }
                Event::Resize(columns, lines) => {
                    screen.resize(columns, lines, theme.base_style());
                    // The terminal is re-sized to its panel on the next render.
                }
                Event::Paste(text) => app.paste(&text)?,
                _ => {}
            }
        }

        if app.should_quit() {
            break;
        }
    }

    // Persist the layout/open-files/cursors so the next plain `mj` resumes here. A save failure
    // (e.g. no writable state dir) must not turn a clean quit into an error.
    if persist_session {
        let _ = app.workspace.to_session().save();
    }
    Ok(())
}

/// The byte offset where the identifier ending at `cursor` begins: `cursor` minus the trailing run
/// of identifier characters (alphanumeric or `_`). A completion replaces `start..cursor`, so an
/// empty run (cursor not after an identifier) yields `cursor` itself — the candidate is inserted.
fn identifier_start(text: &str, cursor: usize) -> usize {
    let end = cursor.min(text.len());
    text[..end]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(end, |(index, _)| index)
}

/// Whether `key` is `Ctrl+Shift+P` (the command palette), tolerant of the terminal reporting
/// the letter as either case.
fn is_command_palette(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && key.mods.contains(Mods::SHIFT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'p'))
}

/// Whether `key` is `Shift+Alt+F` (reformat the document — the universal "Format Document"
/// shortcut). Tolerant of how terminals report the chord: some send the `Shift` modifier, others
/// just upper-case the letter. The `Shift` requirement is deliberate, so this never shadows `Alt+f`
/// (the Emacs "forward-word" editing key); `Ctrl` must be absent for the same reason.
fn is_format_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::ALT)
        && !key.mods.contains(Mods::CTRL)
        && matches!(
            key.code,
            KeyCode::Char(c)
                if c.eq_ignore_ascii_case(&'f')
                    && (key.mods.contains(Mods::SHIFT) || c.is_ascii_uppercase())
        )
}

/// Whether `key` is `Shift+F12` (find all references — the companion to `F12` goto-definition).
/// Requires `Shift` and forbids `Ctrl`/`Alt`, so it is distinct from plain `F12` (goto-definition)
/// and is dispatched before the modifier-agnostic terminal toggle. Tolerant of terminals that add
/// other flags to the chord, matching on the `Shift`+function-key shape via [`REFERENCES_KEY`].
fn is_references_key(key: KeyPress) -> bool {
    key.mods.contains(Mods::SHIFT)
        && !key.mods.contains(Mods::CTRL)
        && !key.mods.contains(Mods::ALT)
        && key.code == REFERENCES_KEY.code
}

/// Translates a crossterm key event into a Keymaker [`KeyPress`], if it maps to one.
pub(crate) fn translate(key: KeyEvent) -> Option<KeyPress> {
    if key.kind == KeyEventKind::Release {
        return None; // ignore key-release events (Kitty protocol)
    }

    let mut mods = Mods::NONE;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        mods |= Mods::CTRL;
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        mods |= Mods::ALT;
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        mods |= Mods::SHIFT;
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        mods |= Mods::SUPER;
    }

    let code = match key.code {
        TermKey::Char(c) => KeyCode::Char(c),
        TermKey::Enter => KeyCode::Enter,
        TermKey::Esc => KeyCode::Escape,
        TermKey::Tab => KeyCode::Tab,
        TermKey::Backspace => KeyCode::Backspace,
        TermKey::Delete => KeyCode::Delete,
        TermKey::Insert => KeyCode::Insert,
        TermKey::Left => KeyCode::Left,
        TermKey::Right => KeyCode::Right,
        TermKey::Up => KeyCode::Up,
        TermKey::Down => KeyCode::Down,
        TermKey::Home => KeyCode::Home,
        TermKey::End => KeyCode::End,
        TermKey::PageUp => KeyCode::PageUp,
        TermKey::PageDown => KeyCode::PageDown,
        TermKey::F(n) => KeyCode::Function(n),
        _ => return None,
    };
    Some(KeyPress::new(mods, code))
}

/// Encodes a [`KeyPress`] into the bytes a terminal child expects, if any.
fn encode_key(key: KeyPress) -> Option<Vec<u8>> {
    let ctrl = key.mods.contains(Mods::CTRL);
    let alt = key.mods.contains(Mods::ALT);
    let mut out = Vec::new();

    match key.code {
        KeyCode::Char(c) => {
            if alt {
                out.push(0x1b); // Alt = ESC prefix
            }
            if ctrl {
                match u8::try_from(c) {
                    Ok(byte) => out.push(byte & 0x1f), // Ctrl maps to a control byte
                    Err(_) => push_char(&mut out, c),
                }
            } else {
                push_char(&mut out, c);
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Escape => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Function(_) => return None,
    }

    (!out.is_empty()).then_some(out)
}

/// Appends the UTF-8 encoding of `c` to `out`.
fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::{encode_key, App, TERMINAL_TOGGLE};
    use keymaker::{KeyCode, KeyPress, Mods};
    use majestic_core::{Editor, Workspace};
    use penumbra::{Buffer, Theme};

    #[test]
    fn renders_workspace_with_tab_bar_and_global_status_bar() {
        // With no terminal: row 0 is the workspace tab bar, the editor body sits below it, and
        // the last row is the global status bar (Steelbore accent background).
        let theme = Theme::steelbore();
        let mut editor = Editor::new();
        editor.handle_key(KeyPress::char('h'));
        editor.handle_key(KeyPress::char('i'));

        let mut app = App::new(Workspace::new(editor));
        let mut surface = Buffer::new(60, 6, theme.base_style());
        app.render(&mut surface, &theme);

        // Row 0 is the tab bar; the scratch buffer is listed there.
        let tabs: String = (0..surface.width())
            .filter_map(|x| surface.cell(x, 0).map(|c| c.symbol))
            .collect();
        assert!(
            tabs.contains("scratch"),
            "tab bar lists the buffer: {tabs:?}"
        );
        // Editor content is drawn just below the tab bar.
        assert_eq!(surface.cell(0, 1).unwrap().symbol, 'h');
        assert_eq!(surface.cell(1, 1).unwrap().symbol, 'i');
        // The bottom row is the status bar: accent background, end-anchored F12 hint present.
        let status_row = surface.height() - 1;
        assert_eq!(surface.cell(0, status_row).unwrap().style.bg, theme.accent);
        let tail: String = (0..surface.width())
            .filter_map(|x| surface.cell(x, status_row).map(|c| c.symbol))
            .collect();
        assert!(
            tail.contains("F12"),
            "status bar shows the terminal hint: {tail:?}"
        );
    }

    #[test]
    fn dot_info_opens_in_the_reader_and_q_closes_it() {
        let dir = std::env::temp_dir().join(format!("mj-info-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sample.info");
        std::fs::write(
            &path,
            "\u{001f}\nFile: sample.info,  Node: Top,  Up: (dir)\n\nHello, info world.\n",
        )
        .unwrap();

        let mut app = App::new(Workspace::new(Editor::new()));
        app.open_path(&path);
        assert!(app.info.is_some(), ".info opens in the Info reader");

        // The reader renders its body into the editor region.
        let theme = Theme::steelbore();
        let mut surface = Buffer::new(60, 6, theme.base_style());
        app.render(&mut surface, &theme);
        let mut text = String::new();
        for y in 0..surface.height() {
            for x in 0..surface.width() {
                if let Some(cell) = surface.cell(x, y) {
                    text.push(cell.symbol);
                }
            }
        }
        assert!(text.contains("Hello, info world."), "node body is shown");

        app.handle_key(KeyPress::char('q'), 60, 6).unwrap();
        assert!(app.info.is_none(), "q closes the reader");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn printable_keys_encode_to_utf8() {
        assert_eq!(encode_key(KeyPress::char('a')), Some(b"a".to_vec()));
        assert_eq!(encode_key(KeyPress::char('A')), Some(b"A".to_vec()));
        assert_eq!(
            encode_key(KeyPress::char('é')),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn control_and_named_keys() {
        assert_eq!(encode_key(KeyPress::ctrl('c')), Some(vec![3])); // Ctrl+C -> ETX
        assert_eq!(encode_key(KeyPress::key(KeyCode::Enter)), Some(vec![b'\r']));
        assert_eq!(
            encode_key(KeyPress::key(KeyCode::Backspace)),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode_key(KeyPress::key(KeyCode::Left)),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            encode_key(KeyPress::new(Mods::ALT, KeyCode::Char('x'))),
            Some(vec![0x1b, b'x'])
        );
    }

    #[test]
    fn function_keys_are_not_encoded() {
        assert_eq!(encode_key(KeyPress::key(KeyCode::Function(5))), None);
        assert_eq!(TERMINAL_TOGGLE, KeyCode::Function(12));
    }
}
