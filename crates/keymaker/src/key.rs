// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Key events: modifier sets ([`Mods`]), key codes ([`KeyCode`]), and chords ([`KeyPress`]).
//!
//! These are Majestic's own input types so the core depends on no terminal backend; the
//! `crossterm` layer translates its `KeyEvent` into a [`KeyPress`] when it is wired in.

use std::ops::{BitOr, BitOrAssign};

/// A set of modifier keys, stored as a bitset (so it is cheap, `Copy`, and orderable).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mods(u8);

impl Mods {
    /// No modifiers.
    pub const NONE: Self = Self(0);
    /// Control.
    pub const CTRL: Self = Self(1 << 0);
    /// Alt / Meta.
    pub const ALT: Self = Self(1 << 1);
    /// Shift.
    pub const SHIFT: Self = Self(1 << 2);
    /// Super / Command.
    pub const SUPER: Self = Self(1 << 3);

    /// Returns `true` if `self` contains every modifier in `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns `true` if no modifiers are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Mods {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Mods {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// A physical key, independent of modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyCode {
    /// A character key (the produced character, lower-case for letters).
    Char(char),
    /// Return / Enter.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// Insert.
    Insert,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// A function key `F1`..`F12`.
    Function(u8),
}

/// A complete key chord: a [`KeyCode`] together with its active [`Mods`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyPress {
    /// The active modifiers.
    pub mods: Mods,
    /// The key.
    pub code: KeyCode,
}

impl KeyPress {
    /// Creates a key press from `mods` and `code`.
    #[must_use]
    pub const fn new(mods: Mods, code: KeyCode) -> Self {
        Self { mods, code }
    }

    /// An unmodified character key.
    #[must_use]
    pub const fn char(c: char) -> Self {
        Self::new(Mods::NONE, KeyCode::Char(c))
    }

    /// A `Ctrl`-modified character key.
    #[must_use]
    pub const fn ctrl(c: char) -> Self {
        Self::new(Mods::CTRL, KeyCode::Char(c))
    }

    /// An unmodified named key.
    #[must_use]
    pub const fn key(code: KeyCode) -> Self {
        Self::new(Mods::NONE, code)
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyCode, KeyPress, Mods};

    #[test]
    fn mods_bitset_union_and_contains() {
        let cs = Mods::CTRL | Mods::SHIFT;
        assert!(cs.contains(Mods::CTRL));
        assert!(cs.contains(Mods::SHIFT));
        assert!(!cs.contains(Mods::ALT));
        assert!(Mods::NONE.is_empty());
        assert!(!cs.is_empty());
    }

    #[test]
    fn convenience_constructors_match_explicit() {
        assert_eq!(
            KeyPress::ctrl('s'),
            KeyPress::new(Mods::CTRL, KeyCode::Char('s'))
        );
        assert_eq!(
            KeyPress::char('a'),
            KeyPress::new(Mods::NONE, KeyCode::Char('a'))
        );
        assert_eq!(
            KeyPress::key(KeyCode::Enter),
            KeyPress::new(Mods::NONE, KeyCode::Enter)
        );
    }
}
