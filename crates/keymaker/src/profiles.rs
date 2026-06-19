// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Built-in keybinding profiles. M0 shipped the [`cua`] profile (Standard §8); the [`emacs`]
//! profile lands in M2, followed by Vim and Spacemacs plus the first-run selector. Every profile
//! binds only command names documented in the Oracle catalog (`oracle::COMMANDS`); the
//! `oracle::commands_missing_docs` guard enforces that contract in CI.

use crate::key::{KeyCode, KeyPress, Mods};
use crate::keymap::{Command, Keymap};

/// A built-in keybinding profile, selected by the `keymap` config field or a profile-switch
/// command. The Spacemacs profile lands in a later M2 chunk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// Common User Access — the default (`Ctrl+C/X/V`, arrows). Non-modal.
    #[default]
    Cua,
    /// Classic Emacs — `C-`/`M-` chords and the `C-x` prefix map. Non-modal.
    Emacs,
    /// Modal Vim — Normal / Insert / Visual.
    Vim,
}

impl Profile {
    /// Parses a profile from its config name (`"cua"`, `"emacs"`, `"vim"`), case-insensitively.
    ///
    /// Returns `None` for an unknown name so the caller can warn and keep the current default
    /// rather than guessing (config stays fail-soft).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "cua" => Some(Self::Cua),
            "emacs" => Some(Self::Emacs),
            "vim" => Some(Self::Vim),
            _ => None,
        }
    }

    /// The canonical config name for this profile.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cua => "cua",
            Self::Emacs => "emacs",
            Self::Vim => "vim",
        }
    }
}

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

/// Builds the global Emacs keymap: the classic `C-`/`M-` motion and editing chords plus the
/// `C-x` prefix map. Exercises the prefix tree's multi-key sequences (`C-x C-s`, `C-x C-c`).
///
/// As with [`cua`], printable keys are left unbound so the editor self-inserts them; only the
/// control/meta chords and named keys appear here. Selection in Emacs is mark-based (`C-Space`),
/// which lands with mark mode in a later M2 chunk — for now the shared motion commands move the
/// point without extending.
#[must_use]
pub fn emacs() -> Keymap {
    use KeyCode::{Backspace, Enter, Tab};

    // Single-chord bindings: (chord, command).
    let chords: &[(KeyPress, &str)] = &[
        // Motion (C-f/b/n/p, C-a/e, C-v/M-v).
        (KeyPress::ctrl('f'), "move-right"),
        (KeyPress::ctrl('b'), "move-left"),
        (KeyPress::ctrl('n'), "move-down"),
        (KeyPress::ctrl('p'), "move-up"),
        (KeyPress::ctrl('a'), "move-line-start"),
        (KeyPress::ctrl('e'), "move-line-end"),
        (KeyPress::ctrl('v'), "page-down"),
        (KeyPress::alt('v'), "page-up"),
        // Editing.
        (KeyPress::key(Enter), "insert-newline"),
        (KeyPress::key(Tab), "indent"),
        (KeyPress::key(Backspace), "delete-backward"),
        (KeyPress::ctrl('d'), "delete-forward"),
        (KeyPress::ctrl('/'), "undo"),
        // Kill / yank (C-k kill-line, C-w/M-w kill-region/save, C-y yank).
        (KeyPress::ctrl('k'), "kill-line"),
        (KeyPress::ctrl('w'), "cut"),
        (KeyPress::alt('w'), "copy"),
        (KeyPress::ctrl('y'), "paste"),
        // Incremental search (isearch-forward).
        (KeyPress::ctrl('s'), "find"),
    ];

    // `C-x`-prefixed bindings: (second chord, command). `C-x` is the canonical Emacs prefix.
    let c_x: &[(KeyPress, &str)] = &[
        (KeyPress::ctrl('s'), "save"),
        (KeyPress::ctrl('c'), "quit"),
        (KeyPress::char('k'), "close-buffer"),
        (KeyPress::char('h'), "select-all"),
    ];

    let mut keymap = Keymap::new();
    for &(key, command) in chords {
        keymap = keymap.bind(&[key], Command::new(command));
    }
    for &(second, command) in c_x {
        keymap = keymap.bind(&[KeyPress::ctrl('x'), second], Command::new(command));
    }
    keymap
}

/// Builds the Vim **Normal**-mode keymap: `hjkl` motion, `i`/`v` to switch mode, and the common
/// normal-mode editing chords. Printable keys not bound here do nothing in Normal mode — the
/// editor only self-inserts while the [`EditMode`](../../majestic_core/index.html) is `Insert`.
#[must_use]
pub fn vim_normal() -> Keymap {
    bind_all(&[
        // Motion.
        (KeyPress::char('h'), "move-left"),
        (KeyPress::char('j'), "move-down"),
        (KeyPress::char('k'), "move-up"),
        (KeyPress::char('l'), "move-right"),
        (KeyPress::char('0'), "move-line-start"),
        (KeyPress::char('$'), "move-line-end"),
        // Mode switches.
        (KeyPress::char('i'), "enter-insert-mode"),
        (KeyPress::char('v'), "enter-visual-mode"),
        // Editing.
        (KeyPress::char('x'), "delete-forward"),
        (KeyPress::char('u'), "undo"),
        (KeyPress::char('p'), "paste"),
    ])
}

/// Builds the Vim **Insert**-mode keymap: `Esc` returns to Normal, the named keys edit, and
/// printable keys self-insert (so only the non-printing keys are bound here).
#[must_use]
pub fn vim_insert() -> Keymap {
    use KeyCode::{Backspace, Down, Enter, Escape, Left, Right, Tab, Up};

    bind_all(&[
        (KeyPress::key(Escape), "enter-normal-mode"),
        (KeyPress::key(Enter), "insert-newline"),
        (KeyPress::key(Tab), "indent"),
        (KeyPress::key(Backspace), "delete-backward"),
        (KeyPress::key(Left), "move-left"),
        (KeyPress::key(Right), "move-right"),
        (KeyPress::key(Up), "move-up"),
        (KeyPress::key(Down), "move-down"),
    ])
}

/// Builds the Vim **Visual**-mode keymap: `hjkl` extend the selection, `y`/`x` copy/cut it, and
/// `Esc`/`v` return to Normal mode.
#[must_use]
pub fn vim_visual() -> Keymap {
    use KeyCode::Escape;

    bind_all(&[
        // Motion extends the selection.
        (KeyPress::char('h'), "select-left"),
        (KeyPress::char('j'), "select-down"),
        (KeyPress::char('k'), "select-up"),
        (KeyPress::char('l'), "select-right"),
        // Operate on the selection.
        (KeyPress::char('y'), "copy"),
        (KeyPress::char('x'), "cut"),
        // Back to Normal mode.
        (KeyPress::key(Escape), "enter-normal-mode"),
        (KeyPress::char('v'), "enter-normal-mode"),
    ])
}

/// Builds a keymap binding each `(chord, command)` as a single-key sequence — the shared tail of
/// the single-chord profile builders.
fn bind_all(bindings: &[(KeyPress, &str)]) -> Keymap {
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

    #[test]
    fn emacs_binds_motion_and_kill_chords() {
        use super::emacs;
        for (key, expected) in [
            (KeyPress::ctrl('f'), "move-right"),
            (KeyPress::ctrl('a'), "move-line-start"),
            (KeyPress::ctrl('k'), "kill-line"),
            (KeyPress::alt('w'), "copy"),
            (KeyPress::ctrl('y'), "paste"),
        ] {
            assert_eq!(
                emacs().lookup(&[key]),
                Lookup::Bound(Command::new(expected))
            );
        }
    }

    #[test]
    fn emacs_c_x_is_a_prefix_resolving_to_commands() {
        use super::emacs;
        let keymap = emacs();
        // `C-x` alone is an incomplete prefix; `C-x C-s` completes to `save`.
        assert_eq!(keymap.lookup(&[KeyPress::ctrl('x')]), Lookup::Prefix);
        assert_eq!(
            keymap.lookup(&[KeyPress::ctrl('x'), KeyPress::ctrl('s')]),
            Lookup::Bound(Command::new("save"))
        );
        assert_eq!(
            keymap.lookup(&[KeyPress::ctrl('x'), KeyPress::char('k')]),
            Lookup::Bound(Command::new("close-buffer"))
        );
    }

    #[test]
    fn emacs_leaves_printable_keys_unbound_for_self_insert() {
        use super::emacs;
        assert_eq!(emacs().lookup(&[KeyPress::char('a')]), Lookup::Unbound);
    }

    #[test]
    fn vim_normal_binds_motion_and_mode_switches() {
        use super::vim_normal;
        let keymap = vim_normal();
        for (key, expected) in [
            (KeyPress::char('h'), "move-left"),
            (KeyPress::char('l'), "move-right"),
            (KeyPress::char('i'), "enter-insert-mode"),
            (KeyPress::char('v'), "enter-visual-mode"),
            (KeyPress::char('x'), "delete-forward"),
        ] {
            assert_eq!(keymap.lookup(&[key]), Lookup::Bound(Command::new(expected)));
        }
        // A key with no normal-mode meaning stays unbound — Normal mode must not self-insert it.
        assert_eq!(keymap.lookup(&[KeyPress::char('z')]), Lookup::Unbound);
    }

    #[test]
    fn vim_insert_and_visual_return_to_normal_on_escape() {
        use super::{vim_insert, vim_visual};
        let escape = KeyPress::key(KeyCode::Escape);
        assert_eq!(
            vim_insert().lookup(&[escape]),
            Lookup::Bound(Command::new("enter-normal-mode"))
        );
        assert_eq!(
            vim_visual().lookup(&[escape]),
            Lookup::Bound(Command::new("enter-normal-mode"))
        );
        // Visual-mode motion extends the selection.
        assert_eq!(
            vim_visual().lookup(&[KeyPress::char('l')]),
            Lookup::Bound(Command::new("select-right"))
        );
    }

    #[test]
    fn profile_round_trips_through_its_name() {
        use super::Profile;
        for profile in [Profile::Cua, Profile::Emacs, Profile::Vim] {
            assert_eq!(Profile::from_name(profile.name()), Some(profile));
        }
        // Case-insensitive, whitespace-tolerant; unknown names fall through to `None`.
        assert_eq!(Profile::from_name("  VIM "), Some(Profile::Vim));
        assert_eq!(Profile::from_name("spacemacs"), None);
        assert_eq!(Profile::default(), Profile::Cua);
    }
}
