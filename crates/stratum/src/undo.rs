// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The branching [`UndoTree`] — non-destructive, Emacs `undo-tree`-style history.
//!
//! Each node holds the document [`Rope`] at that point in history. Because a rope snapshot
//! is an `Arc` bump that shares structure with its neighbours, a whole tree of states costs
//! little more than the edits between them. [`UndoTree::record`] adds a child of the current
//! node; [`UndoTree::undo`] steps to the parent (remembering the branch it left, so
//! [`UndoTree::redo`] returns there); and — crucially — making a new edit after an undo adds
//! a *new branch* rather than discarding the redo path. [`UndoTree::goto`] and
//! [`UndoTree::iter_nodes`] expose the structure for an `undo-tree-visualize` view.
//!
//! Each node also records the [`Edit`] that produced it from its parent; persistence (per
//! the contract, nodes reference edit-journal offsets) is layered on with the journal.
//!
//! # Examples
//! ```
//! use stratum::{Rope, UndoTree};
//!
//! let mut history = UndoTree::new(Rope::new());
//! for ch in ["a", "b", "c"] {
//!     let end = history.current_rope().len_bytes();
//!     let (next, edit) = history.current_rope().edit(end..end, ch);
//!     history.record(next, edit);
//! }
//! assert_eq!(history.current_rope().to_string(), "abc");
//! history.undo();
//! assert_eq!(history.current_rope().to_string(), "ab");
//! history.redo();
//! assert_eq!(history.current_rope().to_string(), "abc");
//! ```

use crate::anchor::Edit;
use crate::rope::Rope;

/// Identifies a node within an [`UndoTree`]. Only the owning tree can mint these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(usize);

impl NodeId {
    /// The node's arena index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

/// A read-only view of one history node, for visualization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// This node's identifier.
    pub id: NodeId,
    /// The parent node, or `None` for the root.
    pub parent: Option<NodeId>,
    /// The edit that produced this node from its parent, or `None` for the root.
    pub edit: Option<Edit>,
}

#[derive(Clone, Debug)]
struct UndoNode {
    rope: Rope,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    last_child: Option<NodeId>,
    edit: Option<Edit>,
}

/// A branching, non-destructive document history.
#[derive(Clone, Debug)]
pub struct UndoTree {
    nodes: Vec<UndoNode>,
    current: NodeId,
}

impl UndoTree {
    /// Creates a history rooted at `initial`.
    #[must_use]
    pub fn new(initial: Rope) -> Self {
        let root = UndoNode {
            rope: initial,
            parent: None,
            children: Vec::new(),
            last_child: None,
            edit: None,
        };
        Self {
            nodes: vec![root],
            current: NodeId(0),
        }
    }

    /// The node the history is currently positioned at.
    #[must_use]
    pub fn current(&self) -> NodeId {
        self.current
    }

    /// The document state at the current node.
    #[must_use]
    pub fn current_rope(&self) -> &Rope {
        &self.nodes[self.current.0].rope
    }

    /// Total number of nodes recorded so far.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if there is a parent to undo to.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.nodes[self.current.0].parent.is_some()
    }

    /// Returns `true` if there is a child to redo to.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        let node = &self.nodes[self.current.0];
        node.last_child.is_some() || !node.children.is_empty()
    }

    /// Records `rope` (produced from the current state by `edit`) as a new child and moves
    /// to it. Returns the new node's id. After an undo, this starts a new branch.
    pub fn record(&mut self, rope: Rope, edit: Edit) -> NodeId {
        let id = NodeId(self.nodes.len());
        let parent = self.current;
        self.nodes.push(UndoNode {
            rope,
            parent: Some(parent),
            children: Vec::new(),
            last_child: None,
            edit: Some(edit),
        });
        let parent_node = &mut self.nodes[parent.0];
        parent_node.children.push(id);
        parent_node.last_child = Some(id);
        self.current = id;
        id
    }

    /// Steps to the parent node, returning its document state, or `None` at the root.
    ///
    /// Remembers the branch just left so a following [`UndoTree::redo`] returns to it.
    pub fn undo(&mut self) -> Option<&Rope> {
        let parent = self.nodes[self.current.0].parent?;
        let from = self.current;
        self.nodes[parent.0].last_child = Some(from);
        self.current = parent;
        Some(&self.nodes[parent.0].rope)
    }

    /// Steps to the remembered (or only) child, returning its state, or `None` at a leaf.
    pub fn redo(&mut self) -> Option<&Rope> {
        let node = &self.nodes[self.current.0];
        let child = node.last_child.or_else(|| node.children.last().copied())?;
        self.current = child;
        Some(&self.nodes[child.0].rope)
    }

    /// Jumps directly to `id`, making it the current node (for visualizer navigation).
    ///
    /// # Panics
    /// Panics if `id` does not belong to this tree.
    pub fn goto(&mut self, id: NodeId) {
        assert!(id.0 < self.nodes.len(), "goto: unknown node {id:?}");
        if let Some(parent) = self.nodes[id.0].parent {
            self.nodes[parent.0].last_child = Some(id);
        }
        self.current = id;
    }

    /// Iterates every node as a [`NodeInfo`], in creation order, for visualization.
    pub fn iter_nodes(&self) -> impl Iterator<Item = NodeInfo> + '_ {
        self.nodes.iter().enumerate().map(|(i, node)| NodeInfo {
            id: NodeId(i),
            parent: node.parent,
            edit: node.edit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::UndoTree;
    use crate::Rope;

    /// Appends `text` at the end of the current state and records it.
    fn type_text(history: &mut UndoTree, text: &str) {
        let end = history.current_rope().len_bytes();
        let (next, edit) = history.current_rope().edit(end..end, text);
        history.record(next, edit);
    }

    #[test]
    fn linear_undo_and_redo() {
        let mut history = UndoTree::new(Rope::new());
        type_text(&mut history, "a");
        type_text(&mut history, "b");
        type_text(&mut history, "c");
        assert_eq!(history.current_rope().to_string(), "abc");

        assert_eq!(
            history.undo().map(ToString::to_string).as_deref(),
            Some("ab")
        );
        assert_eq!(
            history.undo().map(ToString::to_string).as_deref(),
            Some("a")
        );
        assert_eq!(history.undo().map(ToString::to_string).as_deref(), Some(""));
        assert!(!history.can_undo());
        assert!(history.undo().is_none());

        assert_eq!(
            history.redo().map(ToString::to_string).as_deref(),
            Some("a")
        );
        assert_eq!(
            history.redo().map(ToString::to_string).as_deref(),
            Some("ab")
        );
        assert_eq!(
            history.redo().map(ToString::to_string).as_deref(),
            Some("abc")
        );
        assert!(!history.can_redo());
        assert!(history.redo().is_none());
    }

    #[test]
    fn new_edit_after_undo_branches_without_losing_redo() {
        let mut history = UndoTree::new(Rope::new());
        type_text(&mut history, "a"); // "a"
        type_text(&mut history, "b"); // "ab"
        let ab_node = history.current();
        assert_eq!(history.current_rope().to_string(), "ab");

        history.undo(); // -> "a"
        history.undo(); // -> "" (root)
        assert_eq!(history.current_rope().to_string(), "");

        type_text(&mut history, "Z"); // new branch off root: node 3 ("Z")
        assert_eq!(history.current_rope().to_string(), "Z");
        // The old branch still exists: 4 nodes total ("", "a", "ab", "Z").
        assert_eq!(history.node_count(), 4);

        // Jump back to the preserved "ab" state via the visualizer path.
        history.goto(ab_node);
        assert_eq!(history.current_rope().to_string(), "ab");
    }

    #[test]
    fn boundaries_report_no_move() {
        let mut history = UndoTree::new(Rope::from("seed"));
        assert!(!history.can_undo());
        assert!(history.undo().is_none());
        assert!(!history.can_redo());
        assert!(history.redo().is_none());
        assert_eq!(history.iter_nodes().count(), 1);
    }

    /// Tiny deterministic PRNG (xorshift64*), mirroring the other stratum test harnesses.
    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                return 0;
            }
            usize::try_from(self.next_u64() % n as u64).unwrap_or(0)
        }
    }

    /// On a single chain, undo/redo must agree with a positional model at every step.
    #[test]
    fn linear_navigation_matches_model() {
        let mut history = UndoTree::new(Rope::new());
        let mut chain = vec![String::new()];
        for _ in 0..50 {
            type_text(&mut history, "x");
            chain.push(history.current_rope().to_string());
        }

        let mut rng = Rng(0x0DDB_A115);
        let mut pos = chain.len() - 1;
        for _ in 0..1000 {
            if rng.below(2) == 0 {
                let moved = history.undo().map(ToString::to_string);
                if pos == 0 {
                    assert_eq!(moved, None);
                } else {
                    pos -= 1;
                    assert_eq!(moved.as_deref(), Some(chain[pos].as_str()));
                }
            } else {
                let moved = history.redo().map(ToString::to_string);
                if pos == chain.len() - 1 {
                    assert_eq!(moved, None);
                } else {
                    pos += 1;
                    assert_eq!(moved.as_deref(), Some(chain[pos].as_str()));
                }
            }
            assert_eq!(history.current_rope().to_string(), chain[pos]);
        }
    }
}
