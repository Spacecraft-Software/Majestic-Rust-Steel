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

use keymaker::{KeyCode, KeyPress, Mods};
use majestic_core::{Action, Editor, FileTree, Finder, HelpOverlay, InfoReader, Workspace};
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
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
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
            focus: Focus::Editor,
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

    /// Draws the sidebar, the editor area, the bottom terminal panel (when present), and the
    /// status bar — the full UI.md layout.
    fn render(&mut self, surface: &mut Buffer, theme: &Theme) {
        surface.clear(theme.base_style());
        let (body, status) = surface.area().split_bottom(1);
        let main = self.render_sidebar(surface, body, theme);

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
            draw_panel_tab(surface, divider, theme, self.focus == Focus::Terminal);
            if let Some(term) = self.terminal.as_ref() {
                term.render_in(surface, panel_area, theme, self.focus == Focus::Terminal);
            }
        } else {
            self.workspace
                .render(surface, main, theme, self.focus == Focus::Editor);
        }

        self.draw_status_bar(surface, status.y, theme);

        // Modal overlays are drawn last, over everything else.
        let area = surface.area();
        if let Some(finder) = self.finder.as_ref() {
            finder.render(surface, area, theme);
        }
        if let Some(help) = self.help.as_ref() {
            help.render(surface, area, theme);
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
    persist_session: bool,
) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let theme = Theme::steelbore();
    let (columns, lines) = terminal::size()?;
    let mut screen = Screen::new(columns, lines, theme.base_style());
    let mut out = io::stdout();
    let mut app = App::new(workspace);
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

/// Whether `key` is `Ctrl+Shift+P` (the command palette), tolerant of the terminal reporting
/// the letter as either case.
fn is_command_palette(key: KeyPress) -> bool {
    key.mods.contains(Mods::CTRL)
        && key.mods.contains(Mods::SHIFT)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'p'))
}

/// Translates a crossterm key event into a Keymaker [`KeyPress`], if it maps to one.
fn translate(key: KeyEvent) -> Option<KeyPress> {
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
