// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Built-in keybinding profiles. M0 ships the [`cua`] profile (Standard §8); the Emacs, Vim,
//! and Spacemacs profiles and the first-run selector land in M2.

use crate::key::{KeyCode, KeyPress, Mods};
use crate::keymap::{Command, Keymap};

/// Builds the global CUA keymap: the standard editing chords plus cursor motion.
///
/// Printable keys are intentionally unbound — the editor self-inserts any key the active
/// layers do not claim, so only the control chords and named keys appear here.
#[must_use]
pub fn cua() -> Keymap {
    use KeyCode::{
        Backspace, Delete, Down, End, Enter, Home, Left, PageDown, PageUp, Right, Tab, Up,
    };

    let bindings: &[(KeyPress, &str)] = &[
        // CUA clipboard / history / file (Standard §8).
        (KeyPress::ctrl('c'), "copy"),
        (KeyPress::ctrl('x'), "cut"),
        (KeyPress::ctrl('v'), "paste"),
        (KeyPress::ctrl('z'), "undo"),
        (KeyPress::ctrl('y'), "redo"),
        (KeyPress::ctrl('s'), "save"),
        (KeyPress::ctrl('a'), "select-all"),
        (KeyPress::ctrl('f'), "find"),
        (KeyPress::ctrl('q'), "quit"),
        (KeyPress::ctrl('w'), "close-buffer"),
        // Editing keys.
        (KeyPress::key(Enter), "insert-newline"),
        (KeyPress::key(Tab), "indent"),
        (KeyPress::key(Backspace), "delete-backward"),
        (KeyPress::key(Delete), "delete-forward"),
        // Cursor motion.
        (KeyPress::key(Left), "move-left"),
        (KeyPress::key(Right), "move-right"),
        (KeyPress::key(Up), "move-up"),
        (KeyPress::key(Down), "move-down"),
        (KeyPress::key(Home), "move-line-start"),
        (KeyPress::key(End), "move-line-end"),
        (KeyPress::key(PageUp), "page-up"),
        (KeyPress::key(PageDown), "page-down"),
        // Shift+motion extends the selection.
        (KeyPress::new(Mods::SHIFT, Left), "select-left"),
        (KeyPress::new(Mods::SHIFT, Right), "select-right"),
        (KeyPress::new(Mods::SHIFT, Up), "select-up"),
        (KeyPress::new(Mods::SHIFT, Down), "select-down"),
    ];

    let mut keymap = Keymap::new();
    for &(key, command) in bindings {
        keymap = keymap.bind(&[key], Command::new(command));
    }
    keymap
}

#[cfg(test)]
mod tests {
    use super::cua;
    use crate::key::{KeyCode, KeyPress};
    use crate::keymap::{Command, Lookup};

    #[test]
    fn cua_binds_the_core_editing_chords() {
        let keymap = cua();
        for (key, expected) in [
            (KeyPress::ctrl('s'), "save"),
            (KeyPress::ctrl('c'), "copy"),
            (KeyPress::ctrl('v'), "paste"),
            (KeyPress::ctrl('z'), "undo"),
            (KeyPress::key(KeyCode::Left), "move-left"),
            (KeyPress::key(KeyCode::Enter), "insert-newline"),
        ] {
            assert_eq!(keymap.lookup(&[key]), Lookup::Bound(Command::new(expected)));
        }
    }

    #[test]
    fn cua_leaves_printable_keys_unbound_for_self_insert() {
        assert_eq!(cua().lookup(&[KeyPress::char('a')]), Lookup::Unbound);
    }
}
