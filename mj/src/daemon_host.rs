// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The interactive session daemon and its `mj attach` client (PRD #1 §6.8, Unix-only).
//!
//! `mj daemon start` runs [`serve`]: it owns a live editing session ([`SessionHost`]) and mirrors
//! it to any number of attached clients at once (tmux/screen style). A single **broker** thread —
//! the only one that touches the `!Send` session — applies every client's input to the one shared
//! editor and broadcasts each rendered frame to all of them, sizing the mirror to the smallest
//! attached terminal so the content fits every client. An accept thread funnels new connections and
//! one-shot control requests to the broker over a channel; a per-client reader thread funnels that
//! client's input. The broker is **timer-driven**: it renders both on input and on a tick
//! (mirroring `run`'s 16 ms/200 ms cadence), so background output — the integrated terminal, async
//! highlighting, LSP diagnostics — repaints while attached, not only on the next keystroke. Idle
//! ticks that change nothing send nothing, and a session with no clients sleeps until the next
//! attach. Detaching (or losing) one client leaves the session and the other clients running; the
//! session is persisted when the last client leaves, so it survives a daemon restart.
//!
//! [`attach`] is the thin client: it puts its own terminal in raw mode, forwards key/resize
//! events, and paints the bytes the daemon pushes. Because the daemon sends frames on its own
//! schedule (not just in reply to input), the client reads frames on a dedicated thread while the
//! main thread forwards input. `Ctrl-]` detaches (the session keeps running).

use std::collections::HashMap;
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

use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use penumbra::{render, Buffer};
use serde::{Deserialize, Serialize};

use majestic_core::Workspace;
use majestic_daemon::{read_frame, socket_path, write_frame, Daemon, Request, Response};

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

/// Identifies one attached client within a session's mirror set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ClientId(u64);

/// One attached terminal in the mirror set: where to send its frames, its size, and the frame it
/// last displayed — so the broker pushes it only the diff, or a full repaint when it first joins or
/// the shared geometry changes (its `front` then differs in size from the session frame).
#[derive(Debug)]
struct Client {
    writer: UnixStream,
    cols: u16,
    rows: u16,
    front: Buffer,
}

/// An event delivered to the session [`broker`]. The accept thread and the per-client reader
/// threads move only sockets and plain data across the channel — never the `!Send` session, which
/// stays pinned to the broker thread.
#[derive(Debug)]
enum Event {
    /// A client attached at the given terminal size; `writer` is where its frames are sent.
    Attach {
        id: ClientId,
        writer: UnixStream,
        cols: u16,
        rows: u16,
    },
    /// Input arrived from an attached client.
    Input { id: ClientId, input: AttachInput },
    /// A client's reader thread ended (its socket closed): drop it from the mirror set.
    Disconnect { id: ClientId },
    /// A one-shot control request (status / save / shutdown); the broker answers it over `reply`.
    Control {
        request: Request,
        reply: mpsc::Sender<Response>,
    },
}

/// Runs the interactive session daemon on the default socket until a `Shutdown` control request.
///
/// The accept thread is detached: it lives until the process exits when the [`broker`] returns on
/// shutdown.
///
/// # Errors
/// Returns an I/O error binding the socket, or propagated from the broker applying input.
pub fn serve() -> io::Result<()> {
    let listener = bind(&socket_path())?;
    let (events, inbox) = mpsc::channel::<Event>();
    thread::spawn(move || accept_loop(&listener, &events));
    broker(Daemon::bootstrap(), &inbox)
}

/// Accepts connections and funnels them to the broker: a one-shot control request is answered
/// synchronously (the broker replies over a channel), while an `Attach` hands the broker the frame
/// sink and spawns a reader thread for that client's input.
fn accept_loop(listener: &UnixListener, events: &mpsc::Sender<Event>) {
    let mut next_id: u64 = 0;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue; // a failed accept: skip it and keep listening
        };
        match read_frame::<_, Request>(&mut stream) {
            Ok(Request::Attach { cols, rows }) => {
                let id = ClientId(next_id);
                next_id = next_id.wrapping_add(1);
                let Ok(reader) = stream.try_clone() else {
                    continue; // cannot split the socket into read/write halves: drop this client
                };
                let attach = Event::Attach {
                    id,
                    writer: stream,
                    cols,
                    rows,
                };
                if events.send(attach).is_err() {
                    return; // the broker is gone
                }
                let to_broker = events.clone();
                thread::spawn(move || read_client(id, reader, &to_broker));
            }
            Ok(request) => {
                let (reply, answer) = mpsc::channel();
                if events.send(Event::Control { request, reply }).is_err() {
                    return;
                }
                // Block for the broker's answer, then write it back to this one-shot client.
                if let Ok(response) = answer.recv() {
                    let _ = write_frame(&mut stream, &response);
                }
            }
            // A client that hung up before sending anything: just move on.
            Err(_) => {}
        }
    }
}

/// Reads one client's input frames and forwards them to the broker, ending on a clean detach or a
/// closed socket (which becomes an [`Event::Disconnect`] so the broker drops the client).
fn read_client(id: ClientId, mut reader: UnixStream, events: &mpsc::Sender<Event>) {
    while let Ok(input) = read_frame::<_, AttachInput>(&mut reader) {
        let detaching = matches!(input, AttachInput::Detach);
        if events.send(Event::Input { id, input }).is_err() || detaching {
            return; // the broker is gone, or we just forwarded the detach: stop reading
        }
    }
    let _ = events.send(Event::Disconnect { id });
}

/// What woke the broker: an event to handle, a render tick, or the channel closing.
enum Wake {
    Event(Event),
    Tick,
    Closed,
}

/// Waits for the next broker wake-up. With clients attached it polls on a tick (fast while a shell
/// streams output, slower when only editing — matching `run`); with no clients it blocks, so an
/// unwatched session sleeps rather than rendering to nobody.
fn next_wake(inbox: &mpsc::Receiver<Event>, terminal_running: bool, idle: bool) -> Wake {
    if idle {
        return match inbox.recv() {
            Ok(event) => Wake::Event(event),
            Err(_) => Wake::Closed,
        };
    }
    let timeout = if terminal_running {
        Duration::from_millis(16)
    } else {
        Duration::from_millis(200)
    };
    match inbox.recv_timeout(timeout) {
        Ok(event) => Wake::Event(event),
        Err(mpsc::RecvTimeoutError::Timeout) => Wake::Tick,
        Err(mpsc::RecvTimeoutError::Disconnected) => Wake::Closed,
    }
}

/// Owns the session and its mirror set, applying every client's input to the one shared
/// [`SessionHost`] and broadcasting each rendered frame to all attached clients. This is the only
/// thread that touches the `!Send` session, so rendering is serial — a single shared editor state,
/// where parallel rendering would mean cloning the whole `App`; the cheap per-client frame diff in
/// [`broadcast`] makes that unnecessary. The session is persisted when the last client leaves or the
/// editor quits, so it survives a daemon restart.
///
/// # Errors
/// Propagates an I/O error from applying input to the integrated terminal panel, or from diffing a
/// frame.
fn broker(mut daemon: Daemon, inbox: &mpsc::Receiver<Event>) -> io::Result<()> {
    let mut host: Option<SessionHost> = None;
    let mut clients: HashMap<ClientId, Client> = HashMap::new();

    loop {
        let terminal_running = host.as_ref().is_some_and(SessionHost::terminal_running);
        match next_wake(inbox, terminal_running, clients.is_empty()) {
            // Every sender dropped (the accept thread and all readers are gone): the daemon is done.
            Wake::Closed => return Ok(()),
            // A background tick: fall through to broadcast any change (shell output, highlighting).
            Wake::Tick => {}
            Wake::Event(Event::Attach {
                id,
                writer,
                cols,
                rows,
            }) => {
                let session = host.get_or_insert_with(|| {
                    SessionHost::new(Workspace::from_session(daemon.session()), cols, rows)
                });
                // An empty front buffer differs in size from the session frame, so this client's
                // first diff is a full repaint.
                let front = Buffer::new(0, 0, penumbra::Theme::steelbore().base_style());
                clients.insert(
                    id,
                    Client {
                        writer,
                        cols,
                        rows,
                        front,
                    },
                );
                renegotiate(session, &clients);
            }
            Wake::Event(Event::Input {
                input: AttachInput::Key(key),
                ..
            }) => {
                let quit = match (host.as_mut(), translate(key)) {
                    (Some(session), Some(press)) => session.input(press)?,
                    _ => false,
                };
                if quit {
                    end_session(&mut daemon, &mut host, &mut clients);
                    continue; // the session is over; nothing left to broadcast
                }
            }
            Wake::Event(Event::Input {
                id,
                input: AttachInput::Resize { cols, rows },
            }) => {
                if let Some(client) = clients.get_mut(&id) {
                    client.cols = cols;
                    client.rows = rows;
                }
                if let Some(session) = host.as_mut() {
                    renegotiate(session, &clients);
                }
            }
            // A clean detach or a dropped connection: forget that client, leaving the session and
            // the other clients running.
            Wake::Event(
                Event::Input {
                    id,
                    input: AttachInput::Detach,
                }
                | Event::Disconnect { id },
            ) => {
                forget_client(&mut daemon, &mut host, &mut clients, id);
            }
            Wake::Event(Event::Control { request, reply }) => {
                let shutting_down = matches!(request, Request::Shutdown);
                let response = daemon.handle(request);
                let _ = reply.send(response);
                if shutting_down {
                    if let Some(session) = host.as_ref() {
                        daemon.set_session(session.to_session());
                        let _ = daemon.handle(Request::Save);
                    }
                    return Ok(());
                }
            }
        }

        // Mirror the current frame to every client; nothing to render with no one watching.
        if !clients.is_empty() {
            if let Some(session) = host.as_mut() {
                broadcast(session, &mut clients)?;
            }
        }
    }
}

/// Sizes the mirror to the smallest attached terminal (tmux/screen convention) so the session's
/// content fits every client. A no-op when no clients remain.
fn renegotiate(session: &mut SessionHost, clients: &HashMap<ClientId, Client>) {
    let cols = clients.values().map(|client| client.cols).min();
    let rows = clients.values().map(|client| client.rows).min();
    if let (Some(cols), Some(rows)) = (cols, rows) {
        session.resize(cols, rows);
    }
}

/// Drops a client from the mirror set; the session and the other clients keep running. Dropping the
/// client closes its socket, so its painter thread sees EOF and the `mj attach` process exits. When
/// the last client leaves, the session is persisted so it survives a daemon restart.
fn forget_client(
    daemon: &mut Daemon,
    host: &mut Option<SessionHost>,
    clients: &mut HashMap<ClientId, Client>,
    id: ClientId,
) {
    clients.remove(&id);
    if let Some(session) = host.as_mut() {
        renegotiate(session, clients);
        if clients.is_empty() {
            daemon.set_session(session.to_session());
            let _ = daemon.handle(Request::Save);
        }
    }
}

/// Ends the session after the editor quits: persist the final layout, tell every client to restore
/// its terminal and exit (`Ended`), and drop the session.
fn end_session(
    daemon: &mut Daemon,
    host: &mut Option<SessionHost>,
    clients: &mut HashMap<ClientId, Client>,
) {
    if let Some(session) = host.take() {
        daemon.set_session(session.to_session());
        let _ = daemon.handle(Request::Save);
    }
    for mut client in clients.drain().map(|(_, client)| client) {
        let _ = write_frame(&mut client.writer, &ServerFrame::Ended);
    }
}

/// Renders the session once and pushes each client the diff against the frame it last displayed —
/// a full repaint for a freshly attached client (its front buffer is empty), a minimal delta for
/// the rest, and nothing at all for a client whose view is unchanged. A client whose write fails
/// (its terminal went away) is dropped from the mirror set.
///
/// # Errors
/// Propagates a write error from diffing into the in-memory frame buffer.
fn broadcast(session: &mut SessionHost, clients: &mut HashMap<ClientId, Client>) -> io::Result<()> {
    let frame = session.render_frame();
    let mut gone: Vec<ClientId> = Vec::new();
    for (id, client) in clients.iter_mut() {
        let mut bytes = Vec::new();
        render(&client.front, frame, &mut bytes)?;
        if bytes.is_empty() {
            continue; // this client's view did not change this frame
        }
        if write_frame(&mut client.writer, &ServerFrame::Output(bytes)).is_err() {
            gone.push(*id);
            continue;
        }
        client.front.clone_from(frame);
    }
    for id in gone {
        clients.remove(&id);
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
            TermEvent::Key(key) if is_detach(&key) => {
                let _ = write_frame(writer, &AttachInput::Detach);
                return Ok(());
            }
            TermEvent::Key(key) => AttachInput::Key(key),
            TermEvent::Resize(cols, rows) => AttachInput::Resize { cols, rows },
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
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use majestic_core::{LayoutNode, PaneState, Session};
    use majestic_daemon::{read_frame, Daemon};

    use super::{broker, AttachInput, ClientId, Event, ServerFrame};

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

    /// A no-modifier key press, ready to send as [`Event::Input`].
    fn key(ch: char) -> AttachInput {
        AttachInput::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
    }

    /// Reads the next `Output` frame's bytes from a client socket (panics on an unexpected end).
    fn next_output(client: &mut UnixStream) -> Vec<u8> {
        match read_frame::<_, ServerFrame>(client).unwrap() {
            ServerFrame::Output(bytes) => bytes,
            ServerFrame::Ended => panic!("unexpected session end"),
        }
    }

    /// Spawns a broker over a fresh scratch session and returns its event sender + join handle.
    /// Dropping the sender (after all clients detach) closes the channel so the broker returns.
    fn spawn_broker(tag: &str) -> (mpsc::Sender<Event>, thread::JoinHandle<()>) {
        let (events, inbox) = mpsc::channel::<Event>();
        let path = std::env::temp_dir().join(format!("majestic-{tag}-{}.json", std::process::id()));
        let handle =
            thread::spawn(move || broker(Daemon::new(scratch_session(), path), &inbox).unwrap());
        (events, handle)
    }

    /// Drives the broker as a synthetic client: attach, type a character, and confirm the rendered
    /// frame bytes contain it.
    #[test]
    fn attached_client_sees_its_keystrokes_rendered() {
        let (events, handle) = spawn_broker("attach");
        let (mut client, server) = UnixStream::pair().unwrap();
        let id = ClientId(0);

        events
            .send(Event::Attach {
                id,
                writer: server,
                cols: 40,
                rows: 8,
            })
            .unwrap();
        let _ = next_output(&mut client); // initial full frame

        events
            .send(Event::Input {
                id,
                input: key('Z'),
            })
            .unwrap();
        assert!(
            next_output(&mut client).contains(&b'Z'),
            "the rendered frame should contain the typed character"
        );

        events
            .send(Event::Input {
                id,
                input: AttachInput::Detach,
            })
            .unwrap();
        drop(events);
        handle.join().unwrap();
    }

    /// After the initial frame, a session with no input must stay silent: background ticks that
    /// produce no diff send nothing, so an attached client sees no needless traffic.
    #[test]
    fn an_idle_session_pushes_no_frames() {
        let (events, handle) = spawn_broker("idle");
        let (mut client, server) = UnixStream::pair().unwrap();
        let id = ClientId(0);

        events
            .send(Event::Attach {
                id,
                writer: server,
                cols: 40,
                rows: 8,
            })
            .unwrap();
        let _ = next_output(&mut client); // initial full frame

        // No input across several tick intervals: no further frame should be pushed.
        client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let next = read_frame::<_, ServerFrame>(&mut client);
        assert!(
            next.is_err(),
            "an idle daemon must not stream empty frames (got {next:?})"
        );
        client.set_read_timeout(None).unwrap();

        events
            .send(Event::Input {
                id,
                input: AttachInput::Detach,
            })
            .unwrap();
        drop(events);
        handle.join().unwrap();
    }

    /// Two clients attached to one session both see edits typed on either — the defining property
    /// of a mirrored attach.
    #[test]
    fn two_clients_see_the_same_mirrored_session() {
        let (events, handle) = spawn_broker("mirror");
        let (mut a_client, a_server) = UnixStream::pair().unwrap();
        let (mut b_client, b_server) = UnixStream::pair().unwrap();
        let (a, b) = (ClientId(0), ClientId(1));

        for (id, server) in [(a, a_server), (b, b_server)] {
            events
                .send(Event::Attach {
                    id,
                    writer: server,
                    cols: 40,
                    rows: 8,
                })
                .unwrap();
        }
        // Each client gets its own initial full frame.
        let _ = next_output(&mut a_client);
        let _ = next_output(&mut b_client);

        // A character typed on A is mirrored to BOTH clients.
        events
            .send(Event::Input {
                id: a,
                input: key('Q'),
            })
            .unwrap();
        assert!(
            next_output(&mut a_client).contains(&b'Q'),
            "the typing client sees its character"
        );
        assert!(
            next_output(&mut b_client).contains(&b'Q'),
            "the mirrored client sees it too"
        );

        for id in [a, b] {
            events
                .send(Event::Input {
                    id,
                    input: AttachInput::Detach,
                })
                .unwrap();
        }
        drop(events);
        handle.join().unwrap();
    }

    /// Detaching one client leaves the session and the remaining client running and still mirrored.
    #[test]
    fn detaching_one_client_keeps_the_other_running() {
        let (events, handle) = spawn_broker("detach-one");
        let (mut a_client, a_server) = UnixStream::pair().unwrap();
        let (mut b_client, b_server) = UnixStream::pair().unwrap();
        let (a, b) = (ClientId(0), ClientId(1));

        for (id, server) in [(a, a_server), (b, b_server)] {
            events
                .send(Event::Attach {
                    id,
                    writer: server,
                    cols: 40,
                    rows: 8,
                })
                .unwrap();
        }
        let _ = next_output(&mut a_client);
        let _ = next_output(&mut b_client);

        // A detaches; B must keep receiving mirrored edits.
        events
            .send(Event::Input {
                id: a,
                input: AttachInput::Detach,
            })
            .unwrap();
        events
            .send(Event::Input {
                id: b,
                input: key('R'),
            })
            .unwrap();
        assert!(
            next_output(&mut b_client).contains(&b'R'),
            "the remaining client keeps seeing edits after the other detaches"
        );

        events
            .send(Event::Input {
                id: b,
                input: AttachInput::Detach,
            })
            .unwrap();
        drop(events);
        handle.join().unwrap();
    }
}
