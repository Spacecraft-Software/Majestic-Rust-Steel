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
use std::time::Duration;

use crossterm::cursor;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as TermKey, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use keymaker::{KeyCode, KeyPress, Mods};
use majestic_core::Editor;
use majestic_term::PtyTerminal;
use penumbra::{Buffer, Rect, Screen, Style, Theme};

/// The `F12` key spawns/toggles the integrated terminal (reassignable once the Architect lands at M3).
const TERMINAL_TOGGLE: KeyCode = KeyCode::Function(12);

/// Default height of the bottom terminal panel in rows (UI.md §5: 8–15, resizable later).
const PANEL_ROWS: u16 = 10;

/// Editor rows kept visible above the panel; below this total the panel is hidden.
const MIN_EDITOR_ROWS: u16 = 3;

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
    Terminal,
}

/// The running application: the editor plus an optional integrated terminal.
struct App {
    editor: Editor,
    terminal: Option<PtyTerminal>,
    focus: Focus,
}

impl App {
    fn new(editor: Editor) -> Self {
        Self {
            editor,
            terminal: None,
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
        self.editor.should_quit()
    }

    /// Draws the editor area, the bottom terminal panel (when present), and the status bar.
    fn render(&mut self, surface: &mut Buffer, theme: &Theme) {
        surface.clear(theme.base_style());
        let (body, status) = surface.area().split_bottom(1);

        if self.terminal.is_some() && body.height > MIN_EDITOR_ROWS + 1 {
            let panel_rows = PANEL_ROWS.min(body.height - (MIN_EDITOR_ROWS + 1));
            let (editor_area, block) = body.split_bottom(panel_rows + 1);
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

            self.editor
                .render_in(surface, editor_area, theme, self.focus == Focus::Editor);
            draw_panel_tab(surface, divider, theme, self.focus == Focus::Terminal);
            if let Some(term) = self.terminal.as_ref() {
                term.render_in(surface, panel_area, theme);
            }
        } else {
            self.editor.render_in(surface, body, theme, true);
        }

        self.draw_status_bar(surface, status.y, theme);
    }

    /// Draws the global status bar: the editor's status line plus a focus/terminal hint.
    fn draw_status_bar(&self, surface: &mut Buffer, row: u16, theme: &Theme) {
        let style = Style::new(theme.background, theme.accent);
        for x in 0..surface.width() {
            surface.set_char(x, row, ' ', style);
        }
        surface.set_str(0, row, &self.editor.status_line(), style);

        let hint = match (self.terminal.is_some(), self.focus) {
            (false, _) => "[F12: terminal]",
            (true, Focus::Editor) => "[F12: ⇄ TERMINAL]",
            (true, Focus::Terminal) => "[F12: ⇄ EDITOR]",
        };
        if let Ok(len) = u16::try_from(hint.chars().count()) {
            if len < surface.width() {
                surface.set_str(surface.width() - len - 1, row, hint, style);
            }
        }
    }

    fn handle_key(&mut self, key: KeyPress, columns: u16, lines: u16) -> io::Result<()> {
        if key.code == TERMINAL_TOGGLE {
            self.toggle_terminal(columns, lines);
            return Ok(());
        }
        if self.focus == Focus::Terminal {
            if let Some(term) = self.terminal.as_mut() {
                if let Some(bytes) = encode_key(key) {
                    term.write_input(&bytes)?;
                }
                return Ok(());
            }
        }
        self.editor.handle_key(key);
        Ok(())
    }

    fn paste(&mut self, text: &str) -> io::Result<()> {
        if self.focus == Focus::Terminal {
            if let Some(term) = self.terminal.as_mut() {
                return term.write_input(text.as_bytes());
            }
        }
        for ch in text.chars() {
            self.editor.self_insert(ch);
        }
        Ok(())
    }

    /// `F12`: spawn the shell on first use (focusing it), otherwise flip focus between the
    /// editor and the (still-running) terminal panel — the shell session survives toggling.
    fn toggle_terminal(&mut self, columns: u16, lines: u16) {
        if self.terminal_running() {
            self.focus = match self.focus {
                Focus::Editor => Focus::Terminal,
                Focus::Terminal => Focus::Editor,
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
/// # Errors
/// Returns any terminal I/O error from setup, reading events, or rendering.
pub(crate) fn run(editor: Editor) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let theme = Theme::steelbore();
    let (columns, lines) = terminal::size()?;
    let mut screen = Screen::new(columns, lines, theme.base_style());
    let mut out = io::stdout();
    let mut app = App::new(editor);

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
    Ok(())
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
    use majestic_core::Editor;
    use penumbra::{Buffer, Theme};

    #[test]
    fn renders_editor_body_with_a_global_status_bar() {
        // With no terminal, the editor fills the body and a status bar occupies the last row,
        // drawn in the Steelbore accent (Steel Blue) background.
        let theme = Theme::steelbore();
        let mut editor = Editor::new();
        editor.handle_key(KeyPress::char('h'));
        editor.handle_key(KeyPress::char('i'));

        let mut app = App::new(editor);
        let mut surface = Buffer::new(24, 5, theme.base_style());
        app.render(&mut surface, &theme);

        // Editor content is drawn at the top-left of the body.
        assert_eq!(surface.cell(0, 0).unwrap().symbol, 'h');
        assert_eq!(surface.cell(1, 0).unwrap().symbol, 'i');
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
