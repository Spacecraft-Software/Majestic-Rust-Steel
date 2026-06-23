// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Oracle — Majestic's live help & introspection (Standard INVARIANT; PRD #1 §5.2.2).
//!
//! Oracle answers `describe-*`/`apropos` queries by reading the **live** registries the editor
//! actually runs from — the command catalog ([`COMMANDS`]) and a [`keymaker::Keymap`] — so help
//! is always accurate for the running image, never a stale hand-written copy. Every command
//! carries a docstring at registration; [`undocumented_commands`] is the CI lint that fails the
//! build if any does not (PRD §5.2.2).
//!
//! v1 (M1) renders plain text for the six commands `describe-key`, `describe-function`,
//! `describe-variable`, `describe-mode`, `describe-bindings`, and `apropos`. Live hyperlinks into
//! the Steel image (jump-to-definition, current variable values) land with the extension image at
//! M2.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).

use keymaker::{KeyCode, KeyPress, Keymap, Lookup, Mods, Profile};

/// A documented editor command — one entry of the live command registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandDoc {
    /// The command name (the contract shared with the command table that runs it).
    pub name: &'static str,
    /// A one-line docstring shown by `describe-function`/`apropos`.
    pub summary: &'static str,
}

/// A documented configuration variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VariableDoc {
    /// The variable name as written in the manifest.
    pub name: &'static str,
    /// A one-line docstring shown by `describe-variable`.
    pub summary: &'static str,
}

/// The canonical command catalog with docstrings — the registry Oracle and the command palette
/// both read. `majestic-core`'s `Editor::execute` handles exactly these names.
pub const COMMANDS: &[CommandDoc] = &[
    CommandDoc {
        name: "move-left",
        summary: "Move the cursor one column left.",
    },
    CommandDoc {
        name: "move-right",
        summary: "Move the cursor one column right.",
    },
    CommandDoc {
        name: "move-up",
        summary: "Move the cursor one line up.",
    },
    CommandDoc {
        name: "move-down",
        summary: "Move the cursor one line down.",
    },
    CommandDoc {
        name: "move-line-start",
        summary: "Move the cursor to the start of the line.",
    },
    CommandDoc {
        name: "move-line-end",
        summary: "Move the cursor to the end of the line.",
    },
    CommandDoc {
        name: "page-up",
        summary: "Scroll and move the cursor up one page.",
    },
    CommandDoc {
        name: "page-down",
        summary: "Scroll and move the cursor down one page.",
    },
    CommandDoc {
        name: "select-left",
        summary: "Extend the selection one column left.",
    },
    CommandDoc {
        name: "select-right",
        summary: "Extend the selection one column right.",
    },
    CommandDoc {
        name: "select-up",
        summary: "Extend the selection one line up.",
    },
    CommandDoc {
        name: "select-down",
        summary: "Extend the selection one line down.",
    },
    CommandDoc {
        name: "select-all",
        summary: "Select the whole buffer.",
    },
    CommandDoc {
        name: "delete-backward",
        summary: "Delete the character before the cursor (Backspace).",
    },
    CommandDoc {
        name: "delete-forward",
        summary: "Delete the character after the cursor (Delete).",
    },
    CommandDoc {
        name: "insert-newline",
        summary: "Insert a line break at the cursor.",
    },
    CommandDoc {
        name: "indent",
        summary: "Insert one indent (tab-width spaces).",
    },
    CommandDoc {
        name: "undo",
        summary: "Undo the last edit.",
    },
    CommandDoc {
        name: "redo",
        summary: "Redo the last undone edit.",
    },
    CommandDoc {
        name: "copy",
        summary: "Copy the selection to the clipboard.",
    },
    CommandDoc {
        name: "cut",
        summary: "Cut the selection to the clipboard.",
    },
    CommandDoc {
        name: "kill-line",
        summary: "Kill from the cursor to the end of the line (Emacs C-k).",
    },
    CommandDoc {
        name: "paste",
        summary: "Paste the clipboard at the cursor.",
    },
    CommandDoc {
        name: "enter-insert-mode",
        summary: "Enter Vim insert mode (keys insert text).",
    },
    CommandDoc {
        name: "enter-normal-mode",
        summary: "Enter Vim normal mode (motion and operators).",
    },
    CommandDoc {
        name: "enter-visual-mode",
        summary: "Enter Vim visual mode (motion extends the selection).",
    },
    CommandDoc {
        name: "profile-cua",
        summary: "Switch to the CUA keybinding profile.",
    },
    CommandDoc {
        name: "profile-emacs",
        summary: "Switch to the Emacs keybinding profile.",
    },
    CommandDoc {
        name: "profile-vim",
        summary: "Switch to the Vim keybinding profile.",
    },
    CommandDoc {
        name: "save",
        summary: "Save the active buffer to its file.",
    },
    CommandDoc {
        name: "find",
        summary:
            "Incremental search within the buffer (type to match, ↑/↓ to step, Enter accepts).",
    },
    CommandDoc {
        name: "quit",
        summary: "Quit the editor.",
    },
    CommandDoc {
        name: "close-buffer",
        summary: "Close the focused pane.",
    },
];

/// The configuration variables Oracle documents (mirrors `majestic-config`'s `Config`).
pub const VARIABLES: &[VariableDoc] = &[
    VariableDoc {
        name: "theme",
        summary: "Palette token set (e.g. \"steelbore\").",
    },
    VariableDoc {
        name: "keymap",
        summary: "Keybinding profile name (e.g. \"cua\").",
    },
    VariableDoc {
        name: "tab_width",
        summary: "Indent width in columns (clamped 1..=16).",
    },
];

/// Every command name in the catalog (for the command palette and cross-checks).
#[must_use]
pub fn command_names() -> Vec<&'static str> {
    COMMANDS.iter().map(|command| command.name).collect()
}

/// Looks up a command's documentation by name.
#[must_use]
pub fn command_doc(name: &str) -> Option<&'static CommandDoc> {
    COMMANDS.iter().find(|command| command.name == name)
}

/// Looks up a variable's documentation by name.
#[must_use]
pub fn variable_doc(name: &str) -> Option<&'static VariableDoc> {
    VARIABLES.iter().find(|variable| variable.name == name)
}

/// `describe-function`: a command's docstring and the keys currently bound to it (from `keymap`).
#[must_use]
pub fn describe_function(keymap: &Keymap, name: &str) -> String {
    let Some(doc) = command_doc(name) else {
        return format!("No command named `{name}`. Try `apropos`.");
    };
    let keys: Vec<String> = keymap
        .bindings()
        .iter()
        .filter(|(_, command)| command.name() == name)
        .map(|(sequence, _)| format_chord(sequence))
        .collect();
    let bound = if keys.is_empty() {
        "not bound to any key".to_owned()
    } else {
        format!("bound to {}", keys.join(", "))
    };
    format!("{name} — {}\n  {bound}.", doc.summary)
}

/// `describe-key`: the command a key sequence runs in `keymap`, with its docstring.
#[must_use]
pub fn describe_key(keymap: &Keymap, sequence: &[KeyPress]) -> String {
    let chord = format_chord(sequence);
    match keymap.lookup(sequence) {
        Lookup::Bound(command) => {
            let summary = command_doc(command.name()).map_or("(undocumented)", |doc| doc.summary);
            format!("{chord} runs `{}` — {summary}", command.name())
        }
        Lookup::Prefix => format!("{chord} is an incomplete prefix key."),
        Lookup::Unbound => format!("{chord} is not bound."),
    }
}

/// `describe-bindings`: every key binding in `keymap`, key sequence → command, in key order.
#[must_use]
pub fn describe_bindings(keymap: &Keymap) -> String {
    let mut lines = vec!["Key bindings:".to_owned()];
    for (sequence, command) in keymap.bindings() {
        lines.push(format!(
            "  {:<18} {}",
            format_chord(&sequence),
            command.name()
        ));
    }
    lines.join("\n")
}

/// `describe-variable`: a configuration variable's docstring.
#[must_use]
pub fn describe_variable(name: &str) -> String {
    variable_doc(name).map_or_else(
        || format!("No variable named `{name}`."),
        |doc| format!("{} (variable) — {}", doc.name, doc.summary),
    )
}

/// `describe-mode`: a short description of the active keybinding `profile`.
#[must_use]
pub fn describe_mode(profile: Profile) -> String {
    match profile {
        Profile::Cua => {
            "CUA — Common User Access: Ctrl+C/X/V copy/cut/paste, Ctrl+Z/Y undo/redo, Ctrl+S \
             save, arrows move. The non-modal default."
        }
        Profile::Emacs => {
            "Emacs — C-/M- chords and the C-x prefix map: C-f/b/n/p motion, C-k kill-line, \
             C-w/M-w/C-y kill/save/yank, C-x C-s save, C-x C-c quit. Non-modal."
        }
        Profile::Vim => {
            "Vim — modal: Normal (hjkl motion, i/v switch), Insert (Esc returns to Normal), \
             Visual (hjkl extends the selection, y/x copy/cut)."
        }
        Profile::Spacemacs => {
            "Spacemacs — Vim modality with a SPC leader menu in Normal mode (SPC f s save, \
             SPC f f find, SPC b d close-buffer, SPC q q quit); which-key lists the options."
        }
    }
    .to_owned()
}

/// `apropos`: commands whose name or docstring contains `query` (case-insensitive).
#[must_use]
pub fn apropos(query: &str) -> String {
    let needle = query.to_lowercase();
    let matches: Vec<String> = COMMANDS
        .iter()
        .filter(|command| {
            command.name.to_lowercase().contains(&needle)
                || command.summary.to_lowercase().contains(&needle)
        })
        .map(|command| format!("  {:<18} {}", command.name, command.summary))
        .collect();
    if matches.is_empty() {
        return format!("No commands match `{query}`.");
    }
    format!("Commands matching `{query}`:\n{}", matches.join("\n"))
}

/// The profile↔catalog guard: command names bound in `keymap` that are absent from [`COMMANDS`].
///
/// Every keybinding profile must bind only documented commands, so a built-in profile and the
/// catalog can never drift (and a name typo in a profile is caught at test time rather than
/// surfacing as a silent "unbound command" at runtime). An empty result means the profile is
/// compliant. Names are returned sorted and de-duplicated.
#[must_use]
pub fn commands_missing_docs(keymap: &Keymap) -> Vec<String> {
    let mut missing: Vec<String> = keymap
        .bindings()
        .iter()
        .map(|(_, command)| command.name())
        .filter(|name| command_doc(name).is_none())
        .map(str::to_owned)
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}

/// The CI docstring lint: command names whose docstring is empty (PRD §5.2.2). Empty means a
/// command was registered without documentation — a build failure.
#[must_use]
pub fn undocumented_commands() -> Vec<&'static str> {
    COMMANDS
        .iter()
        .filter(|command| command.summary.trim().is_empty())
        .map(|command| command.name)
        .collect()
}

/// Renders a key sequence as a human-readable chord (e.g. `Ctrl+s`, `Ctrl+x Ctrl+c`, `Left`).
fn format_chord(sequence: &[KeyPress]) -> String {
    sequence
        .iter()
        .map(|key| format_key(*key))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders one keypress: modifier prefixes plus the key name.
fn format_key(key: KeyPress) -> String {
    let mut rendered = String::new();
    if key.mods.contains(Mods::CTRL) {
        rendered.push_str("Ctrl+");
    }
    if key.mods.contains(Mods::ALT) {
        rendered.push_str("Alt+");
    }
    if key.mods.contains(Mods::SHIFT) {
        rendered.push_str("Shift+");
    }
    if key.mods.contains(Mods::SUPER) {
        rendered.push_str("Super+");
    }
    let name = match key.code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Escape => "Esc".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Insert => "Insert".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        KeyCode::Function(number) => format!("F{number}"),
    };
    rendered.push_str(&name);
    rendered
}

#[cfg(test)]
mod tests {
    use super::{
        apropos, command_names, commands_missing_docs, describe_bindings, describe_function,
        describe_key, describe_variable, undocumented_commands, COMMANDS,
    };
    use keymaker::{cua, emacs, spacemacs_normal, vim_insert, vim_normal, vim_visual, KeyPress};

    #[test]
    fn every_command_is_documented() {
        // The docstring lint (PRD §5.2.2): no command may be registered without a docstring.
        assert_eq!(undocumented_commands(), Vec::<&str>::new());
    }

    #[test]
    fn command_names_are_unique() {
        let mut names = command_names();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate command name in the catalog");
    }

    #[test]
    fn describe_function_reads_live_bindings() {
        let help = describe_function(&cua(), "save");
        assert!(help.contains("Save the active buffer"));
        assert!(
            help.contains("Ctrl+s"),
            "should show the live CUA binding: {help}"
        );
    }

    #[test]
    fn describe_function_rejects_unknown() {
        assert!(describe_function(&cua(), "frobnicate").contains("No command named"));
    }

    #[test]
    fn describe_key_resolves_against_the_keymap() {
        let help = describe_key(&cua(), &[KeyPress::ctrl('s')]);
        assert!(help.contains("save"));
        assert!(describe_key(&cua(), &[KeyPress::ctrl('j')]).contains("not bound"));
    }

    #[test]
    fn describe_bindings_lists_the_profile() {
        let bindings = describe_bindings(&cua());
        assert!(bindings.contains("copy"));
        assert!(bindings.contains("Ctrl+"));
    }

    #[test]
    fn apropos_matches_name_and_docstring() {
        let result = apropos("select");
        assert!(result.contains("select-all"));
        // Matches docstrings too: "clipboard" appears in copy/cut/paste summaries.
        assert!(apropos("clipboard").contains("paste"));
        assert!(apropos("zzz-nothing").contains("No commands match"));
    }

    #[test]
    fn describe_variable_reads_the_registry() {
        assert!(describe_variable("tab_width").contains("Indent width"));
        assert!(describe_variable("nope").contains("No variable named"));
    }

    #[test]
    fn catalog_is_non_empty() {
        assert!(COMMANDS.len() >= 20);
    }

    #[test]
    fn built_in_profiles_bind_only_documented_commands() {
        // The profile↔catalog guard: every command a built-in profile binds must be documented,
        // so help is complete and a profile name typo can never become a silent runtime miss.
        for keymap in [
            cua(),
            emacs(),
            vim_normal(),
            vim_insert(),
            vim_visual(),
            spacemacs_normal(),
        ] {
            assert_eq!(commands_missing_docs(&keymap), Vec::<String>::new());
        }
    }
}
