// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Layered key [`Dispatcher`] — resolves keys against a stack of [`Keymap`] layers.
//!
//! A [`Dispatcher`] holds layers in priority order (buffer-local → minor-mode → major-mode →
//! global, PRD §5.2.1) and the keys typed so far toward a multi-key binding. Each fed key
//! extends the pending sequence; the highest-priority layer that binds it wins, an
//! incomplete prefix in any layer means "wait for more", and a sequence bound nowhere is
//! reported (so the editor can fall back to self-insert) and the pending sequence resets.

use crate::key::KeyPress;
use crate::keymap::{Command, Keymap, Lookup};

/// What feeding a key to a [`Dispatcher`] produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The pending sequence resolved to this command.
    Command(Command),
    /// The pending sequence is an incomplete prefix; more keys are expected.
    Pending,
    /// The sequence is bound in no layer; here is the full chord that failed.
    Unbound(Vec<KeyPress>),
}

/// Resolves keys against an ordered stack of keymap layers.
#[derive(Clone, Debug)]
pub struct Dispatcher {
    layers: Vec<Keymap>,
    pending: Vec<KeyPress>,
}

impl Dispatcher {
    /// Creates a dispatcher over `layers`, ordered highest priority first.
    #[must_use]
    pub fn new(layers: Vec<Keymap>) -> Self {
        Self {
            layers,
            pending: Vec::new(),
        }
    }

    /// The keys accumulated toward an in-progress multi-key binding.
    #[must_use]
    pub fn pending(&self) -> &[KeyPress] {
        &self.pending
    }

    /// Clears any in-progress sequence.
    pub fn reset(&mut self) {
        self.pending.clear();
    }

    /// Feeds one key and resolves it against the layers.
    pub fn feed(&mut self, key: KeyPress) -> Resolution {
        self.pending.push(key);

        let mut any_prefix = false;
        for layer in &self.layers {
            match layer.lookup(&self.pending) {
                Lookup::Bound(command) => {
                    self.pending.clear();
                    return Resolution::Command(command);
                }
                Lookup::Prefix => any_prefix = true,
                Lookup::Unbound => {}
            }
        }

        if any_prefix {
            Resolution::Pending
        } else {
            Resolution::Unbound(std::mem::take(&mut self.pending))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Dispatcher, Resolution};
    use crate::key::{KeyCode, KeyPress};
    use crate::keymap::{Command, Keymap};

    #[test]
    fn single_chord_resolves_immediately() {
        let global = Keymap::new().bind(&[KeyPress::ctrl('s')], Command::new("save"));
        let mut dispatcher = Dispatcher::new(vec![global]);
        assert_eq!(
            dispatcher.feed(KeyPress::ctrl('s')),
            Resolution::Command(Command::new("save"))
        );
        assert!(dispatcher.pending().is_empty());
    }

    #[test]
    fn multi_key_sequence_pends_then_resolves() {
        let seq = [KeyPress::ctrl('x'), KeyPress::ctrl('s')];
        let global = Keymap::new().bind(&seq, Command::new("save-all"));
        let mut dispatcher = Dispatcher::new(vec![global]);
        assert_eq!(dispatcher.feed(KeyPress::ctrl('x')), Resolution::Pending);
        assert_eq!(dispatcher.pending().len(), 1);
        assert_eq!(
            dispatcher.feed(KeyPress::ctrl('s')),
            Resolution::Command(Command::new("save-all"))
        );
        assert!(dispatcher.pending().is_empty());
    }

    #[test]
    fn unbound_key_reports_chord_and_resets() {
        let global = Keymap::new().bind(&[KeyPress::ctrl('s')], Command::new("save"));
        let mut dispatcher = Dispatcher::new(vec![global]);
        let key = KeyPress::char('q');
        assert_eq!(dispatcher.feed(key), Resolution::Unbound(vec![key]));
        assert!(dispatcher.pending().is_empty());
    }

    #[test]
    fn higher_layer_overrides_lower() {
        let global = Keymap::new().bind(
            &[KeyPress::key(KeyCode::Enter)],
            Command::new("insert-newline"),
        );
        let buffer_local =
            Keymap::new().bind(&[KeyPress::key(KeyCode::Enter)], Command::new("submit"));
        // Buffer-local is higher priority than global.
        let mut dispatcher = Dispatcher::new(vec![buffer_local, global]);
        assert_eq!(
            dispatcher.feed(KeyPress::key(KeyCode::Enter)),
            Resolution::Command(Command::new("submit"))
        );
    }
}
