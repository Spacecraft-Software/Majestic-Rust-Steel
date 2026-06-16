// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The interactive terminal front-end: crossterm raw-mode loop driving the [`Editor`].
//!
//! This is the only place that touches the real terminal. It enables raw mode and the
//! alternate screen (restored on drop, even on panic), reads crossterm events, translates
//! them into Keymaker [`KeyPress`]es, drives the editor, and presents each frame through the
//! Penumbra [`Screen`]. The editor model itself is backend-agnostic and tested headless.

use std::io::{self, Write};

use crossterm::cursor;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as TermKey, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use keymaker::{KeyCode, KeyPress, Mods};
use majestic_core::Editor;
use penumbra::{Screen, Theme};

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

/// Runs the editor's interactive loop until a quit command is issued.
///
/// # Errors
/// Returns any terminal I/O error from setup, reading events, or rendering.
pub(crate) fn run(mut editor: Editor) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let theme = Theme::steelbore();
    let (cols, rows) = terminal::size()?;
    let mut screen = Screen::new(cols, rows, theme.base_style());
    let mut out = io::stdout();

    loop {
        editor.render(screen.back_mut(), &theme);
        screen.present(&mut out)?;
        out.flush()?;

        match event::read()? {
            Event::Key(key) => {
                if let Some(press) = translate(key) {
                    editor.handle_key(press);
                }
            }
            Event::Resize(cols, rows) => screen.resize(cols, rows, theme.base_style()),
            Event::Paste(text) => {
                for ch in text.chars() {
                    editor.self_insert(ch);
                }
            }
            _ => {}
        }

        if editor.should_quit() {
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
