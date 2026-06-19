// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Session persistence (PRD #1 §5.3): the saved window layout, open files, and cursor positions.
//!
//! A [`Session`] is the serializable shape of a [`Workspace`](crate::Workspace): the open panes
//! (file path + cursor + scroll), the binary window-tree layout over them, and which pane was
//! focused. [`Workspace::to_session`](crate::Workspace::to_session) captures it and
//! [`Workspace::from_session`](crate::Workspace::from_session) rebuilds it, so the layout, buffer
//! list, and cursor positions survive a restart. The M2 daemon reuses this to resurrect sessions.
//!
//! The on-disk form is pretty JSON (human-readable and hand-editable).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::Split;

/// A saved editing session: the open panes, their window-tree layout, and the focused pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// One entry per pane, in window-tree in-order; [`LayoutNode::Leaf`] indexes into this.
    pub panes: Vec<PaneState>,
    /// The window-tree layout over `panes`.
    pub layout: LayoutNode,
    /// The in-order ordinal of the focused pane.
    pub focused: usize,
}

/// A single pane's restorable state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneState {
    /// The open file's path, or `None` for an unsaved scratch buffer (restored empty).
    pub path: Option<PathBuf>,
    /// The cursor's byte offset.
    pub cursor: usize,
    /// The viewport's top row.
    pub viewport_top: usize,
    /// The viewport's left column.
    pub viewport_left: usize,
}

/// The window-tree layout, mirroring the workspace's split tree but referencing panes by their
/// in-order ordinal — a stable index into [`Session::panes`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutNode {
    /// A single pane, identified by its ordinal in [`Session::panes`].
    Leaf(usize),
    /// A split of two child layouts along `dir`, `first` taking `ratio` percent of the span.
    Split {
        /// The split axis.
        dir: Split,
        /// Percent of the span given to `first` (the rest, less a divider, goes to `second`).
        ratio: u16,
        /// The first (left/top) child.
        first: Box<LayoutNode>,
        /// The second (right/bottom) child.
        second: Box<LayoutNode>,
    },
}

impl Session {
    /// Serializes this session to pretty JSON.
    ///
    /// # Errors
    /// Returns a `serde_json` error only on an internal serialization bug (this data is plain).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parses a session from JSON.
    ///
    /// # Errors
    /// Returns the `serde_json` error when `json` is not a valid session document.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Writes this session as JSON to `path`, creating parent directories.
    ///
    /// # Errors
    /// Returns an I/O error if the directory cannot be created or the file cannot be written.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = self
            .to_json()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        std::fs::write(path, json)
    }

    /// Reads a session from the JSON file at `path`.
    ///
    /// # Errors
    /// Returns an I/O error if the file cannot be read, or an invalid-data error if the JSON does
    /// not parse as a session.
    pub fn load_from(path: &Path) -> io::Result<Self> {
        let json = std::fs::read_to_string(path)?;
        Self::from_json(&json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// The canonical session file: `$XDG_STATE_HOME/majestic/session.json`, else
    /// `$HOME/.local/state/majestic/session.json`. `None` when neither variable is set.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".local").join("state"))
            })?;
        Some(base.join("majestic").join("session.json"))
    }

    /// Saves this session to [`Session::default_path`], returning the path written.
    ///
    /// # Errors
    /// Returns an I/O error when there is no state home, or when the write fails.
    pub fn save(&self) -> io::Result<PathBuf> {
        let path = Self::default_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state home to write to"))?;
        self.save_to(&path)?;
        Ok(path)
    }

    /// Loads the saved session from [`Session::default_path`], or `None` when there is none (or it
    /// is unreadable / does not parse — a corrupt session never blocks startup).
    #[must_use]
    pub fn load() -> Option<Self> {
        Self::load_from(&Self::default_path()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutNode, PaneState, Session};
    use crate::workspace::Split;

    fn sample() -> Session {
        Session {
            panes: vec![
                PaneState {
                    path: Some("/tmp/a.rs".into()),
                    cursor: 12,
                    viewport_top: 1,
                    viewport_left: 0,
                },
                PaneState {
                    path: None,
                    cursor: 0,
                    viewport_top: 0,
                    viewport_left: 0,
                },
            ],
            layout: LayoutNode::Split {
                dir: Split::Columns,
                ratio: 50,
                first: Box::new(LayoutNode::Leaf(0)),
                second: Box::new(LayoutNode::Leaf(1)),
            },
            focused: 1,
        }
    }

    #[test]
    fn session_round_trips_through_json() {
        let session = sample();
        let restored = Session::from_json(&session.to_json().unwrap()).unwrap();
        assert_eq!(restored, session);
    }

    #[test]
    fn session_round_trips_through_a_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-session-{}", std::process::id()));
        path.push("session.json");
        let session = sample();
        session.save_to(&path).unwrap();
        assert_eq!(Session::load_from(&path).unwrap(), session);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
