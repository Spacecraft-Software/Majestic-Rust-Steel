// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The interactive session daemon and its `mj attach` client (PRD #1 §6.8, Unix-only).
//!
//! `mj daemon start` runs [`serve`]: it owns a live editing session ([`SessionHost`]) and, when a
//! client attaches, streams the rendered terminal bytes and consumes the client's input until it
//! detaches — leaving the session running so it can be re-attached from another TTY. Because the
//! daemon *is* the `mj` binary, it drives the full editor `App` directly. v1 renders on input
//! (each keystroke produces one frame) and serves one client at a time; concurrent mirrored
//! clients and timer-driven async frames are a later increment.
//!
//! [`attach`] is the thin client: it puts its own terminal in raw mode, forwards key/resize
//! events, and paints the bytes the daemon returns. `Ctrl-]` detaches (the session keeps running).

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use serde::{Deserialize, Serialize};

use majestic_core::Workspace;
use majestic_daemon::{read_frame, socket_path, write_frame, Daemon, Request};

use crate::tui::{translate, SessionHost, TerminalGuard};

/// A client→daemon message sent while attached.
#[derive(Debug, Serialize, Deserialize)]
enum AttachInput {
    /// A key event from the client's terminal.
    Key(KeyEvent),
    /// The client's terminal resized.
    Resize { cols: u16, rows: u16 },
    /// Detach, leaving the session running.
    Detach,
}

/// A daemon→client message sent while attached.
#[derive(Debug, Serialize, Deserialize)]
enum ServerFrame {
    /// Terminal bytes to write to the client's screen (a render diff).
    Output(Vec<u8>),
    /// The editor quit; the client should restore its terminal and exit.
    Ended,
}

/// `Ctrl-]` detaches the client (the telnet escape convention) — a chord the editor does not bind.
///
/// Legacy terminals send byte `0x1d`, which crossterm reports as `Ctrl-5`; terminals speaking the
/// kitty/`modifyOtherKeys` protocol report it as `Ctrl-]`. Accept both so detach works either way.
fn is_detach(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char(']' | '5'))
}

/// Runs the interactive session daemon on the default socket until a `Shutdown` control request.
///
/// # Errors
/// Returns an I/O error binding the socket or serving a connection.
pub fn serve() -> io::Result<()> {
    let listener = bind(&socket_path())?;
    let mut daemon = Daemon::bootstrap();
    let mut host: Option<SessionHost> = None;
    for stream in listener.incoming() {
        let mut stream = stream?;
        match read_frame::<_, Request>(&mut stream) {
            Ok(Request::Attach { cols, rows }) => {
                serve_attach(&mut daemon, &mut host, &mut stream, cols, rows)?;
            }
            Ok(request) => {
                let response = daemon.handle(request);
                let _ = write_frame(&mut stream, &response);
                if daemon.wants_shutdown() {
                    break;
                }
            }
            // A client that hung up before sending anything: just move on.
            Err(_) => {}
        }
    }
    Ok(())
}

/// Serves one attached client: render-on-input, 1:1 (every input yields exactly one frame), until
/// the client detaches or the editor quits. The live host is reused across attaches; the session
/// is persisted on every detach so it survives a daemon restart.
fn serve_attach(
    daemon: &mut Daemon,
    host: &mut Option<SessionHost>,
    stream: &mut UnixStream,
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let mut session = host
        .take()
        .unwrap_or_else(|| SessionHost::new(Workspace::from_session(daemon.session()), cols, rows));
    session.resize(cols, rows);
    write_frame(stream, &ServerFrame::Output(session.render()?))?;

    let mut ended = false;
    // The loop ends when a frame fails to read (the client disconnected without a clean detach).
    while let Ok(input) = read_frame::<_, AttachInput>(stream) {
        match input {
            AttachInput::Key(key) => {
                let quit = match translate(key) {
                    Some(press) => session.input(press)?,
                    None => false,
                };
                write_frame(stream, &ServerFrame::Output(session.render()?))?;
                if quit {
                    let _ = write_frame(stream, &ServerFrame::Ended);
                    ended = true;
                    break;
                }
            }
            AttachInput::Resize { cols, rows } => {
                session.resize(cols, rows);
                write_frame(stream, &ServerFrame::Output(session.render()?))?;
            }
            AttachInput::Detach => break,
        }
    }

    // Persist the layout/cursors (survives a daemon restart and shows in `mj session`).
    daemon.set_session(session.to_session());
    let _ = daemon.handle(Request::Save);
    if !ended {
        *host = Some(session); // keep the live session for re-attach
    }
    Ok(())
}

/// Binds the listener at `path`, creating its parent directory `0700` and clearing a stale socket.
fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    UnixListener::bind(path)
}

/// Attaches this terminal to the running session daemon, painting its frames and forwarding input
/// until `Ctrl-]` (detach) or the editor quits. Returns `Ok(false)` when no daemon is running.
///
/// # Errors
/// Returns an I/O error connecting, exchanging frames, or driving the terminal.
pub fn attach() -> io::Result<bool> {
    let path = socket_path();
    let mut writer = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let mut reader = writer.try_clone()?;

    let (cols, rows) = terminal::size()?;
    write_frame(&mut writer, &Request::Attach { cols, rows })?;

    let _guard = TerminalGuard::enter()?; // raw mode + alternate screen, restored on drop
    let mut out = io::stdout();
    paint(&mut out, read_frame(&mut reader)?)?; // initial full frame

    loop {
        let message = match event::read()? {
            Event::Key(key) if is_detach(&key) => {
                let _ = write_frame(&mut writer, &AttachInput::Detach);
                break;
            }
            Event::Key(key) => AttachInput::Key(key),
            Event::Resize(cols, rows) => AttachInput::Resize { cols, rows },
            // v1 forwards keys and resizes only; paste/mouse/focus are ignored.
            _ => continue,
        };
        write_frame(&mut writer, &message)?;
        if !paint(&mut out, read_frame(&mut reader)?)? {
            break; // the session ended
        }
    }
    Ok(true)
}

/// Writes a server frame to the terminal; returns `false` when the session has ended.
fn paint(out: &mut impl Write, frame: ServerFrame) -> io::Result<bool> {
    match frame {
        ServerFrame::Output(bytes) => {
            out.write_all(&bytes)?;
            out.flush()?;
            Ok(true)
        }
        ServerFrame::Ended => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use majestic_core::{LayoutNode, PaneState, Session};
    use majestic_daemon::{read_frame, write_frame, Daemon};

    use super::{serve_attach, AttachInput, ServerFrame};

    fn scratch_session() -> Session {
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

    #[test]
    fn attached_client_sees_its_keystrokes_rendered() {
        // Drive `serve_attach` over a socket pair, acting as a synthetic client: attach, type a
        // character, and confirm the rendered frame bytes contain it.
        let (mut client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();

        let host = thread::spawn(move || {
            let path =
                std::env::temp_dir().join(format!("majestic-attach-{}.json", std::process::id()));
            let mut daemon = Daemon::new(scratch_session(), path);
            let mut slot = None;
            serve_attach(&mut daemon, &mut slot, &mut server, 40, 8).unwrap();
        });

        // Initial frame.
        let _: ServerFrame = read_frame(&mut client).unwrap();
        // Type 'Z'; expect it echoed into the rendered frame.
        write_frame(
            &mut client,
            &AttachInput::Key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE)),
        )
        .unwrap();
        let frame: ServerFrame = read_frame(&mut client).unwrap();
        let bytes = match frame {
            ServerFrame::Output(bytes) => bytes,
            ServerFrame::Ended => panic!("unexpected end"),
        };
        assert!(
            bytes.contains(&b'Z'),
            "the rendered frame should contain the typed character"
        );

        // Detach cleanly so the server thread returns.
        write_frame(&mut client, &AttachInput::Detach).unwrap();
        host.join().unwrap();
    }
}
