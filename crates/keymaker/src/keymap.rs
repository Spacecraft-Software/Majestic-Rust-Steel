// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The persistent prefix-tree [`Keymap`] and [`Command`] identifiers.
//!
//! A [`Keymap`] maps a sequence of [`KeyPress`]es to a [`Command`]. It is immutable:
//! [`Keymap::bind`] returns a *new* keymap that copies only the nodes along the changed path
//! and shares every untouched subtree with the original through `Arc`. So a runtime rebind
//! never disturbs a keymap value already being walked by in-flight dispatch (PRD §5.2.1), and
//! profile switches are cheap.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::key::KeyPress;

/// A named editor command that a key sequence resolves to (e.g. `"save"`, `"move-left"`).
///
/// The string is the contract between keymaps and the command table that runs them; the
/// inner `Arc<str>` keeps cloning cheap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command(Arc<str>);

impl Command {
    /// Creates a command identifier from its name.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self(Arc::from(name))
    }

    /// The command's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Command {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

/// The result of resolving a key sequence against a [`Keymap`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The sequence is bound to this command.
    Bound(Command),
    /// The sequence is an incomplete prefix of one or more bindings; more keys are expected.
    Prefix,
    /// The sequence matches no binding.
    Unbound,
}

#[derive(Clone, Debug, Default)]
struct Node {
    command: Option<Command>,
    children: BTreeMap<KeyPress, Arc<Node>>,
}

/// An immutable prefix tree mapping key sequences to [`Command`]s.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    root: Arc<Node>,
}

impl Keymap {
    /// Creates an empty keymap.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a new keymap with `sequence` bound to `command`.
    ///
    /// Copy-on-write: only the nodes on the path to `sequence` are rebuilt; all other
    /// subtrees are shared with `self`, which is left unchanged.
    #[must_use]
    pub fn bind(&self, sequence: &[KeyPress], command: Command) -> Self {
        Self {
            root: Arc::new(bind_node(&self.root, sequence, command)),
        }
    }

    /// Resolves `sequence`: bound, an incomplete prefix, or unbound.
    #[must_use]
    pub fn lookup(&self, sequence: &[KeyPress]) -> Lookup {
        let mut node = &self.root;
        for key in sequence {
            match node.children.get(key) {
                Some(child) => node = child,
                None => return Lookup::Unbound,
            }
        }
        if let Some(command) = &node.command {
            Lookup::Bound(command.clone())
        } else if node.children.is_empty() {
            Lookup::Unbound
        } else {
            Lookup::Prefix
        }
    }
}

fn bind_node(node: &Node, sequence: &[KeyPress], command: Command) -> Node {
    let Some((head, rest)) = sequence.split_first() else {
        return Node {
            command: Some(command),
            children: node.children.clone(),
        };
    };
    let existing = match node.children.get(head) {
        Some(child) => child.as_ref().clone(),
        None => Node::default(),
    };
    let mut children = node.children.clone();
    children.insert(*head, Arc::new(bind_node(&existing, rest, command)));
    Node {
        command: node.command.clone(),
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Keymap, Lookup};
    use crate::key::{KeyCode, KeyPress};

    #[test]
    fn bind_and_lookup_single_chord() {
        let keymap = Keymap::new().bind(&[KeyPress::ctrl('s')], Command::new("save"));
        assert_eq!(
            keymap.lookup(&[KeyPress::ctrl('s')]),
            Lookup::Bound(Command::new("save"))
        );
        assert_eq!(keymap.lookup(&[KeyPress::ctrl('q')]), Lookup::Unbound);
    }

    #[test]
    fn multi_key_sequence_is_a_prefix_then_bound() {
        let seq = [KeyPress::ctrl('x'), KeyPress::ctrl('s')];
        let keymap = Keymap::new().bind(&seq, Command::new("save-some-buffers"));
        assert_eq!(keymap.lookup(&seq[..1]), Lookup::Prefix);
        assert_eq!(
            keymap.lookup(&seq),
            Lookup::Bound(Command::new("save-some-buffers"))
        );
        assert_eq!(
            keymap.lookup(&[KeyPress::ctrl('x'), KeyPress::ctrl('c')]),
            Lookup::Unbound
        );
    }

    #[test]
    fn rebind_does_not_disturb_the_old_value() {
        let base = Keymap::new().bind(&[KeyPress::char('a')], Command::new("alpha"));
        let updated = base
            .bind(&[KeyPress::char('b')], Command::new("bravo"))
            .bind(&[KeyPress::char('a')], Command::new("ALPHA"));

        // The original keymap is untouched (in-flight dispatch would still see it).
        assert_eq!(
            base.lookup(&[KeyPress::char('a')]),
            Lookup::Bound(Command::new("alpha"))
        );
        assert_eq!(base.lookup(&[KeyPress::char('b')]), Lookup::Unbound);
        // The new keymap has both the addition and the override.
        assert_eq!(
            updated.lookup(&[KeyPress::char('a')]),
            Lookup::Bound(Command::new("ALPHA"))
        );
        assert_eq!(
            updated.lookup(&[KeyPress::char('b')]),
            Lookup::Bound(Command::new("bravo"))
        );
    }

    #[test]
    fn named_key_binding() {
        let keymap = Keymap::new().bind(&[KeyPress::key(KeyCode::Enter)], "insert-newline".into());
        assert_eq!(
            keymap.lookup(&[KeyPress::key(KeyCode::Enter)]),
            Lookup::Bound(Command::new("insert-newline"))
        );
    }
}
