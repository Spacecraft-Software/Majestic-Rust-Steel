// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The interactive terminal front-end: a crossterm raw-mode loop driving the editor and an
//! optional integrated terminal.
//!
//! This is the only place that touches the real terminal. It enables raw mode + the alternate
//! screen (restored on drop, even on panic), reads crossterm events, and routes them through
//! an [`App`] that holds the [`Editor`] and a lazily-spawned [`PtyTerminal`]. `F12` toggles
//! focus between editing and a full-screen shell; while the terminal is focused, keys are
//! encoded to bytes and written to the PTY. Each frame is presented through a Penumbra
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
use penumbra::{Buffer, Screen, Theme};

/// The `F12` key toggles the integrated terminal (reassignable once the Architect lands at M3).
const TERMINAL_TOGGLE: KeyCode = KeyCode::Function(12);

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

    /// The terminal is "active" while focused and present.
    fn terminal_active(&self) -> bool {
        self.focus == Focus::Terminal && self.terminal.is_some()
    }

    /// Returns to the editor if the focused shell has exited.
    fn reap_dead_terminal(&mut self) {
        if self.focus == Focus::Terminal
            && self
                .terminal
                .as_ref()
                .is_some_and(|term| !term.is_running())
        {
            self.focus = Focus::Editor;
        }
    }

    fn should_quit(&self) -> bool {
        self.editor.should_quit()
    }

    fn render(&mut self, surface: &mut Buffer, theme: &Theme) {
        if self.focus == Focus::Terminal {
            if let Some(term) = &self.terminal {
                surface.clear(theme.base_style());
                term.render(surface, theme);
                return;
            }
        }
        self.editor.render(surface, theme);
    }

    fn resize(&mut self, columns: u16, lines: u16) {
        if let Some(term) = self.terminal.as_mut() {
            term.resize(usize::from(columns), usize::from(lines));
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

    fn toggle_terminal(&mut self, columns: u16, lines: u16) {
        match self.focus {
            Focus::Terminal => self.focus = Focus::Editor,
            Focus::Editor => {
                let alive = self.terminal.as_ref().is_some_and(PtyTerminal::is_running);
                if !alive {
                    self.terminal =
                        PtyTerminal::spawn(usize::from(columns), usize::from(lines)).ok();
                }
                if self.terminal.is_some() {
                    self.focus = Focus::Terminal;
                }
            }
        }
    }
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

        // Poll quickly while a terminal streams output; idle longer when only editing.
        let timeout = if app.terminal_active() {
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
                    app.resize(columns, lines);
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
    use super::{encode_key, TERMINAL_TOGGLE};
    use keymaker::{KeyCode, KeyPress, Mods};

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
