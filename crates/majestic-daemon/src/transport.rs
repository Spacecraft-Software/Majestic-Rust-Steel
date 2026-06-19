// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Unix-socket transport: the daemon serve loop and the client (Unix-only).
//!
//! [`run`] binds the socket under a `0700` directory and serves a [`Daemon`] until a client sends
//! [`Request::Shutdown`]; [`status`] and [`stop`] are the client side. Connections are served one
//! at a time — sufficient for the control protocol (status / save / shutdown). Concurrent,
//! mirrored interactive clients (the attach surface) arrive in a later WS3 increment on Morpheus.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use crate::protocol::{read_frame, write_frame, DaemonStatus, Request, Response};
use crate::Daemon;

/// Runs the daemon on the default socket ([`crate::socket_path`]) until a client sends Shutdown.
///
/// # Errors
/// Returns an I/O error binding the socket or serving a connection.
pub fn run() -> io::Result<()> {
    let listener = bind(&crate::socket_path())?;
    serve(Daemon::bootstrap(), &listener)
}

/// Queries a running daemon's status, or `Ok(None)` when none is listening.
///
/// # Errors
/// Returns an I/O error if a daemon is listening but the exchange fails.
pub fn status() -> io::Result<Option<DaemonStatus>> {
    status_at(&crate::socket_path())
}

/// Stops a running daemon; `Ok(false)` when none is listening.
///
/// # Errors
/// Returns an I/O error if a daemon is listening but the exchange fails.
pub fn stop() -> io::Result<bool> {
    stop_at(&crate::socket_path())
}

/// Binds the listener at `path`, creating its parent directory `0700` and clearing a stale socket.
fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        // The runtime directory is per-user and private (PRD §6.8: local-only).
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    // A socket file left by a previous run blocks `bind`; remove it (absent is fine).
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    UnixListener::bind(path)
}

/// Serves `daemon` over `listener`, one connection at a time, until a Shutdown is handled.
fn serve(mut daemon: Daemon, listener: &UnixListener) -> io::Result<()> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        if serve_connection(&mut daemon, &mut stream)? {
            break; // a Shutdown was handled
        }
    }
    Ok(())
}

/// Serves one client connection until it disconnects or asks the daemon to shut down. Returns
/// `true` when a Shutdown was handled (the serve loop should then exit).
fn serve_connection(daemon: &mut Daemon, stream: &mut UnixStream) -> io::Result<bool> {
    loop {
        let request: Request = match read_frame(stream) {
            Ok(request) => request,
            // The client closed the connection cleanly — done with this client.
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(error) => return Err(error),
        };
        let response = daemon.handle(request);
        write_frame(stream, &response)?;
        if daemon.wants_shutdown() {
            return Ok(true);
        }
    }
}

/// Sends one request to the daemon at `path` and reads its response (one connection per call).
/// A test helper — the production client paths ([`status_at`]/[`stop_at`]) tolerate "no daemon".
#[cfg(test)]
fn request_at(path: &Path, request: &Request) -> io::Result<Response> {
    let mut stream = UnixStream::connect(path)?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

fn status_at(path: &Path) -> io::Result<Option<DaemonStatus>> {
    match UnixStream::connect(path) {
        Ok(mut stream) => {
            write_frame(&mut stream, &Request::Status)?;
            match read_frame::<_, Response>(&mut stream)? {
                Response::Status(status) => Ok(Some(status)),
                Response::Ok => Ok(None),
                Response::Error(message) => Err(io::Error::other(message)),
            }
        }
        Err(error) if not_running(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn stop_at(path: &Path) -> io::Result<bool> {
    match UnixStream::connect(path) {
        Ok(mut stream) => {
            write_frame(&mut stream, &Request::Shutdown)?;
            let _: Response = read_frame(&mut stream)?;
            Ok(true)
        }
        Err(error) if not_running(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether a connect error means "no daemon is listening" (vs a real failure).
fn not_running(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::{bind, request_at, serve};
    use crate::protocol::{Request, Response};
    use crate::Daemon;
    use majestic_core::{LayoutNode, PaneState, Session};

    fn two_pane_session() -> Session {
        Session {
            panes: vec![
                PaneState {
                    path: Some("/tmp/a.rs".into()),
                    cursor: 0,
                    viewport_top: 0,
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
                dir: majestic_core::Split::Columns,
                ratio: 50,
                first: Box::new(LayoutNode::Leaf(0)),
                second: Box::new(LayoutNode::Leaf(1)),
            },
            focused: 1,
        }
    }

    #[test]
    fn client_round_trips_status_then_shuts_the_daemon_down() {
        let dir = std::env::temp_dir().join(format!("majestic-daemon-it-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("daemon.sock");
        let session_path = dir.join("session.json");
        two_pane_session().save_to(&session_path).unwrap();

        // Bind before spawning so the socket exists when the client connects (no startup race),
        // and bootstrap from the saved file (resurrection over the wire).
        let listener = bind(&socket).unwrap();
        let daemon = Daemon::bootstrap_at(session_path.clone());
        let server = thread::spawn(move || serve(daemon, &listener).unwrap());

        // A client queries status and gets the resurrected 2-pane session.
        match request_at(&socket, &Request::Status).unwrap() {
            Response::Status(status) => {
                assert_eq!(status.panes, 2);
                assert_eq!(status.focused, 1);
            }
            other => panic!("expected Status, got {other:?}"),
        }

        // Shutdown returns Ok and ends the serve loop, so the server thread joins.
        assert_eq!(
            request_at(&socket, &Request::Shutdown).unwrap(),
            Response::Ok
        );
        server.join().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
