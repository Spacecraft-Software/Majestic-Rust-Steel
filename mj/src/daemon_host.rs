// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The interactive session daemon and its `mj attach` client (PRD #1 §6.8, Unix-only).
//!
//! `mj daemon start` runs [`serve`]: it owns a live editing session ([`SessionHost`]) and, when a
//! client attaches, streams the rendered terminal bytes and consumes the client's input until it
//! detaches — leaving the session running so it can be re-attached from another TTY. Because the
//! daemon *is* the `mj` binary, it drives the full editor `App` directly. The attach loop is
//! **timer-driven**: it renders both on input and on a tick (mirroring `run`'s 16 ms/200 ms
//! cadence), so background output — the integrated terminal, async highlighting, LSP diagnostics —
//! repaints while attached, not only on the next keystroke. One client at a time; concurrent
//! mirrored clients remain a later increment.
//!
//! [`attach`] is the thin client: it puts its own terminal in raw mode, forwards key/resize
//! events, and paints the bytes the daemon pushes. Because the daemon sends frames on its own
//! schedule (not just in reply to input), the client reads frames on a dedicated thread while the
//! main thread forwards input. `Ctrl-]` detaches (the session keeps running).

use std::fs;
use std::io::{self, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

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

/// Serves one attached client until it detaches or the editor quits. Renders both on input and on
/// a tick, so background output (terminal panel, async highlighting, LSP diagnostics) keeps
/// painting while attached; idle ticks that produce no diff send nothing. The live host is reused
/// across attaches; the session is persisted on detach so it survives a daemon restart.
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
    // The initial full frame: `resize` blanked the off-screen front buffer, so a re-attached
    // client (whose terminal is blank) gets a complete repaint rather than a stale diff.
    write_frame(stream, &ServerFrame::Output(session.render()?))?;

    // Read client input on a separate thread and deliver it over a channel. The serve loop then
    // waits on the channel with a timeout, so it can fall through and push a background frame when
    // no input arrives — and `read_frame` (which is blocking here) never times out mid-frame, so
    // the wire stays in sync. The reader touches only the socket, never the `!Send` session.
    let (sender, receiver) = mpsc::channel::<AttachInput>();
    let mut input_stream = stream.try_clone()?;
    let reader = thread::spawn(move || {
        while let Ok(input) = read_frame::<_, AttachInput>(&mut input_stream) {
            if sender.send(input).is_err() {
                break; // the serve loop has gone away
            }
        }
    });

    let ended = drive_attached(&mut session, stream, &receiver)?;

    // Unblock the reader thread's pending `read_frame` by closing the connection, then join it.
    let _ = stream.shutdown(Shutdown::Both);
    let _ = reader.join();

    // Persist the layout/cursors (survives a daemon restart and shows in `mj session`).
    daemon.set_session(session.to_session());
    let _ = daemon.handle(Request::Save);
    if !ended {
        *host = Some(session); // keep the live session for re-attach (detach or disconnect)
    }
    Ok(())
}

/// Drives an attached session until it detaches, disconnects, or the editor quits. Returns whether
/// the editor quit (`true` drops the host; detach/disconnect keep it live for re-attach).
fn drive_attached(
    session: &mut SessionHost,
    stream: &mut UnixStream,
    receiver: &mpsc::Receiver<AttachInput>,
) -> io::Result<bool> {
    loop {
        // Poll fast while a shell streams output, idle longer when only editing — mirrors `run`.
        let timeout = if session.terminal_running() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(200)
        };
        let mut quit = false;
        match receiver.recv_timeout(timeout) {
            Ok(AttachInput::Key(key)) => {
                if let Some(press) = translate(key) {
                    quit = session.input(press)?;
                }
            }
            Ok(AttachInput::Resize { cols, rows }) => session.resize(cols, rows),
            // A clean detach, or the reader thread ending (the client hung up): stop serving but
            // keep the live host so the session can be re-attached.
            Ok(AttachInput::Detach) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(false)
            }
            // No input this tick: fall through to render any background change.
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // Push a frame only when something actually changed (an unchanged frame diffs to nothing),
        // so an idle session sends no traffic.
        let bytes = session.render()?;
        if !bytes.is_empty() && write_frame(stream, &ServerFrame::Output(bytes)).is_err() {
            return Ok(false); // client gone
        }
        if quit {
            let _ = write_frame(stream, &ServerFrame::Ended);
            return Ok(true);
        }
    }
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
    let reader = writer.try_clone()?;

    let (cols, rows) = terminal::size()?;
    write_frame(&mut writer, &Request::Attach { cols, rows })?;

    let _guard = TerminalGuard::enter()?; // raw mode + alternate screen, restored on drop

    // The daemon pushes frames on its own schedule (background output, not just replies to input),
    // so paint them on a dedicated thread. `stop` lets either side wind the other down: the painter
    // sets it when the session ends or the daemon disconnects; the input loop polls it.
    let stop = Arc::new(AtomicBool::new(false));
    let painter = spawn_painter(reader, Arc::clone(&stop));

    let result = forward_input(&mut writer, &stop);

    // Closing our half makes the daemon drop us, which ends the painter's blocking read; then join.
    let _ = writer.shutdown(Shutdown::Both);
    let _ = painter.join();
    result.map(|()| true)
}

/// Spawns the frame-painter thread: it reads server frames and writes them to the terminal until
/// the session ends or the daemon disconnects, then signals `stop` so the input loop can exit.
fn spawn_painter(mut reader: UnixStream, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut out = io::stdout();
        // Paint frames until the daemon closes the connection, the session ends (`Ended` →
        // `Ok(false)`), or a terminal write fails.
        while let Ok(frame) = read_frame::<_, ServerFrame>(&mut reader) {
            if !matches!(paint(&mut out, frame), Ok(true)) {
                break;
            }
        }
        stop.store(true, Ordering::Release);
    })
}

/// Forwards terminal input to the daemon until `Ctrl-]` (detach), the painter flags `stop` (the
/// session ended or disconnected), or a write fails. Polls so a `stop` set by the painter is seen
/// promptly even while no key is pressed.
fn forward_input(writer: &mut UnixStream, stop: &AtomicBool) -> io::Result<()> {
    while !stop.load(Ordering::Acquire) {
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let message = match event::read()? {
            Event::Key(key) if is_detach(&key) => {
                let _ = write_frame(writer, &AttachInput::Detach);
                return Ok(());
            }
            Event::Key(key) => AttachInput::Key(key),
            Event::Resize(cols, rows) => AttachInput::Resize { cols, rows },
            // Keys and resizes only; paste/mouse/focus are ignored.
            _ => continue,
        };
        if write_frame(writer, &message).is_err() {
            return Ok(()); // the daemon is gone
        }
    }
    Ok(())
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
    use std::time::Duration;

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

    #[test]
    fn an_idle_session_pushes_no_frames() {
        // After the initial frame, a session with no input must stay silent: background ticks that
        // produce no diff send nothing, so an attached client sees no needless traffic.
        let (mut client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();

        let host = thread::spawn(move || {
            let path =
                std::env::temp_dir().join(format!("majestic-idle-{}.json", std::process::id()));
            let mut daemon = Daemon::new(scratch_session(), path);
            let mut slot = None;
            serve_attach(&mut daemon, &mut slot, &mut server, 40, 8).unwrap();
        });

        // The initial full frame arrives.
        let _: ServerFrame = read_frame(&mut client).unwrap();

        // No input across several tick intervals: no further frame should be pushed.
        client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let next = read_frame::<_, ServerFrame>(&mut client);
        assert!(
            next.is_err(),
            "an idle daemon must not stream empty frames (got {next:?})"
        );

        // Detach cleanly so the server thread returns.
        client.set_read_timeout(None).unwrap();
        write_frame(&mut client, &AttachInput::Detach).unwrap();
        host.join().unwrap();
    }
}
