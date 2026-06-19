// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The [`Daemon`]: a headless server owning a [`Session`], answering client requests.
//!
//! [`Daemon::bootstrap`] loads any saved session so the layout, open files, and cursors survive a
//! restart (PRD §6.8 session resurrection). [`Daemon::handle`] is the pure request→response core;
//! the socket serve loop that drives it lives in the transport layer.

use std::path::PathBuf;

use majestic_core::{LayoutNode, PaneState, Session};

use crate::protocol::{DaemonStatus, Request, Response};

/// A headless server that owns a [`Session`], answers client requests, and persists on demand.
#[derive(Debug)]
pub struct Daemon {
    session: Session,
    session_path: PathBuf,
    shutdown: bool,
}

impl Daemon {
    /// Creates a daemon owning `session`, persisting it to `session_path` on [`Request::Save`].
    #[must_use]
    pub fn new(session: Session, session_path: PathBuf) -> Self {
        Self {
            session,
            session_path,
            shutdown: false,
        }
    }

    /// Boots a daemon owning the session at [`Session::default_path`] (resurrection: the layout,
    /// open files, and cursors survive a restart). Falls back to an empty session when there is no
    /// state home or none has been saved.
    #[must_use]
    pub fn bootstrap() -> Self {
        let session_path =
            Session::default_path().unwrap_or_else(|| PathBuf::from("majestic-session.json"));
        Self::bootstrap_at(session_path)
    }

    /// Like [`Daemon::bootstrap`] but at an explicit `session_path` (used in tests).
    #[must_use]
    pub fn bootstrap_at(session_path: PathBuf) -> Self {
        let session = Session::load_from(&session_path).unwrap_or_else(|_| empty_session());
        Self::new(session, session_path)
    }

    /// The session this daemon owns.
    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Replaces the owned session (e.g. after an attached client edited it).
    pub fn set_session(&mut self, session: Session) {
        self.session = session;
    }

    /// Whether a [`Request::Shutdown`] has been handled (the serve loop should then exit).
    #[must_use]
    pub fn wants_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Handles one request, mutating daemon state and returning the response to send back.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the protocol will grow payload-carrying requests (e.g. Attach{session}) that handlers consume"
    )]
    pub fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::Status => Response::Status(self.status()),
            Request::Save => match self.session.save_to(&self.session_path) {
                Ok(()) => Response::Ok,
                Err(error) => Response::Error(error.to_string()),
            },
            Request::Shutdown => {
                self.shutdown = true;
                Response::Ok
            }
        }
    }

    fn status(&self) -> DaemonStatus {
        DaemonStatus {
            panes: self.session.panes.len(),
            focused: self.session.focused,
            session_path: Some(self.session_path.display().to_string()),
        }
    }
}

/// An empty single-pane session for a fresh daemon (no file opened yet).
fn empty_session() -> Session {
    Session {
        panes: vec![PaneState {
            path: None,
            cursor: 0,
            viewport_top: 0,
            viewport_left: 0,
        }],
        layout: LayoutNode::Leaf(0),
        focused: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{empty_session, Daemon};
    use crate::protocol::{Request, Response};
    use majestic_core::{LayoutNode, PaneState, Session};

    fn pane(path: Option<&str>) -> PaneState {
        PaneState {
            path: path.map(Into::into),
            cursor: 0,
            viewport_top: 0,
            viewport_left: 0,
        }
    }

    fn two_pane_session() -> Session {
        Session {
            panes: vec![pane(Some("/tmp/a.rs")), pane(None)],
            layout: LayoutNode::Split {
                dir: majestic_core::Split::Columns,
                ratio: 50,
                first: Box::new(LayoutNode::Leaf(0)),
                second: Box::new(LayoutNode::Leaf(1)),
            },
            focused: 1,
        }
    }

    #[test]
    fn fresh_daemon_owns_an_empty_single_pane_session() {
        let mut daemon = Daemon::new(empty_session(), "/nonexistent/x.json".into());
        match daemon.handle(Request::Status) {
            Response::Status(status) => {
                assert_eq!(status.panes, 1);
                assert_eq!(status.focused, 0);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_request_latches_the_flag() {
        let mut daemon = Daemon::new(empty_session(), "/nonexistent/x.json".into());
        assert!(!daemon.wants_shutdown());
        assert_eq!(daemon.handle(Request::Shutdown), Response::Ok);
        assert!(daemon.wants_shutdown());
    }

    #[test]
    fn session_survives_a_daemon_restart() {
        // The §6.8 exit criterion at the unit level: save, then a fresh bootstrap at the same path
        // resurrects the identical session.
        let mut path = std::env::temp_dir();
        path.push(format!("majestic-daemon-{}", std::process::id()));
        path.push("session.json");

        let session = two_pane_session();
        let mut daemon = Daemon::new(session.clone(), path.clone());
        assert_eq!(daemon.handle(Request::Save), Response::Ok);

        let mut restarted = Daemon::bootstrap_at(path.clone());
        assert_eq!(restarted.session(), &session);
        match restarted.handle(Request::Status) {
            Response::Status(status) => {
                assert_eq!(status.panes, 2);
                assert_eq!(status.focused, 1);
            }
            other => panic!("expected Status, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
