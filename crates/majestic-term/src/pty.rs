// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! [`PtyTerminal`] — a live child program over a PTY feeding an embedded [`Terminal`].
//!
//! [`PtyTerminal::spawn`] launches the user's `$SHELL` on a pseudo-terminal (via
//! `alacritty_terminal`'s `tty`, so no `unsafe` enters this crate), then runs a background
//! reader thread that pumps the child's output into a shared [`Terminal`]. The reader blocks on
//! a `mio` readiness poll rather than busy-waiting — it sleeps until the PTY has output (or the
//! editor wakes it to shut down). Keystrokes are written back with [`PtyTerminal::write_input`],
//! and [`PtyTerminal::render`] draws the live grid. Dropping it wakes and joins the reader, then
//! hangs up the child (`SIGHUP`).

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, Options, Shell};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use penumbra::{Buffer, Rect, Theme};

use crate::terminal::Terminal;

/// `mio` token for PTY-master read-readiness.
const DATA: Token = Token(0);
/// `mio` token for the shutdown [`Waker`].
const WAKE: Token = Token(1);

/// A live terminal session: a PTY child whose output drives an embedded [`Terminal`].
pub struct PtyTerminal {
    terminal: Arc<Mutex<Terminal>>,
    pty: Option<tty::Pty>,
    writer: Arc<Mutex<File>>,
    reader: Option<JoinHandle<()>>,
    waker: Waker,
    stop: Arc<AtomicBool>,
    columns: usize,
    screen_lines: usize,
}

impl PtyTerminal {
    /// Spawns the default shell (`$SHELL`) on a `columns × screen_lines` PTY.
    ///
    /// # Errors
    /// Returns an I/O error if the PTY or child process cannot be created.
    pub fn spawn(columns: usize, screen_lines: usize) -> io::Result<Self> {
        Self::launch(&Options::default(), columns, screen_lines)
    }

    /// Spawns `program` with `args` on a `columns × screen_lines` PTY.
    ///
    /// # Errors
    /// Returns an I/O error if the PTY or child process cannot be created.
    pub fn spawn_command(
        program: &str,
        args: &[&str],
        columns: usize,
        screen_lines: usize,
    ) -> io::Result<Self> {
        let shell = Shell::new(
            program.to_owned(),
            args.iter().map(|arg| (*arg).to_owned()).collect(),
        );
        let options = Options {
            shell: Some(shell),
            ..Options::default()
        };
        Self::launch(&options, columns, screen_lines)
    }

    fn launch(options: &Options, columns: usize, screen_lines: usize) -> io::Result<Self> {
        let columns = columns.max(1);
        let screen_lines = screen_lines.max(1);
        let terminal = Arc::new(Mutex::new(Terminal::new(columns, screen_lines)));
        let pty = tty::new(options, window_size(columns, screen_lines), 0)?;

        let reader_file = pty.file().try_clone()?;
        // The PTY write side is shared (`Arc<Mutex>`): the UI thread writes keystrokes, and the reader
        // thread writes the emulator's replies to terminal queries — serialized so they never interleave.
        let writer = Arc::new(Mutex::new(pty.file().try_clone()?));
        let reader_terminal = Arc::clone(&terminal);
        let reader_writer = Arc::clone(&writer);

        // Build the poller on the UI thread so the shutdown `Waker` is available to `Drop`; the
        // `Poll` itself moves into the reader thread.
        let poll = Poll::new()?;
        let waker = Waker::new(poll.registry(), WAKE)?;
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader =
            thread::spawn(move || pump(poll, reader_file, &reader_terminal, &reader_stop, &reader_writer));

        Ok(Self {
            terminal,
            pty: Some(pty),
            writer,
            reader: Some(reader),
            waker,
            stop,
            columns,
            screen_lines,
        })
    }

    /// Writes `bytes` (e.g. encoded keystrokes) to the child program.
    ///
    /// # Errors
    /// Returns an I/O error if writing to the PTY fails.
    pub fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Resizes the terminal grid and informs the child via the PTY.
    pub fn resize(&mut self, columns: usize, screen_lines: usize) {
        self.columns = columns.max(1);
        self.screen_lines = screen_lines.max(1);
        lock(&self.terminal).resize(self.columns, self.screen_lines);
        if let Some(pty) = self.pty.as_mut() {
            pty.on_resize(window_size(self.columns, self.screen_lines));
        }
    }

    /// Renders the live terminal grid into `surface`.
    pub fn render(&self, surface: &mut Buffer, theme: &Theme) {
        lock(&self.terminal).render(surface, theme);
    }

    /// Renders the live terminal grid into `area` of `surface` (offset and clipped to it). When
    /// `focused`, a block cursor marks the terminal's cursor position.
    pub fn render_in(&self, surface: &mut Buffer, area: Rect, theme: &Theme, focused: bool) {
        lock(&self.terminal).render_in(surface, area, theme, focused);
    }

    /// Returns `true` while the child program is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.reader
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// The number of columns.
    #[must_use]
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// The number of visible lines.
    #[must_use]
    pub fn screen_lines(&self) -> usize {
        self.screen_lines
    }
}

impl Drop for PtyTerminal {
    fn drop(&mut self) {
        // Ask the reader to stop and wake it out of its blocking poll, then hang up the child.
        self.stop.store(true, Ordering::Release);
        let _ = self.waker.wake();
        self.pty = None; // dropping the Pty SIGHUPs the child
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl fmt::Debug for PtyTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyTerminal")
            .field("columns", &self.columns)
            .field("screen_lines", &self.screen_lines)
            .field("running", &self.is_running())
            .finish_non_exhaustive()
    }
}

/// Locks the shared terminal, recovering from poisoning rather than cascading a panic.
fn lock(terminal: &Mutex<Terminal>) -> std::sync::MutexGuard<'_, Terminal> {
    terminal.lock().unwrap_or_else(PoisonError::into_inner)
}

fn window_size(columns: usize, screen_lines: usize) -> WindowSize {
    WindowSize {
        num_cols: u16::try_from(columns).unwrap_or(u16::MAX),
        num_lines: u16::try_from(screen_lines).unwrap_or(u16::MAX),
        cell_width: 1,
        cell_height: 1,
    }
}

/// Reads child output until the child exits, feeding it into the shared terminal.
///
/// The PTY master is non-blocking (`alacritty_terminal` sets `O_NONBLOCK`). Rather than spin on
/// `WouldBlock`, the reader blocks in `mio`'s readiness poll and only runs when the master has
/// output or the [`Waker`] fires for shutdown. On each readable wake it drains all available
/// bytes; when the child exits, the master reports EOF/`EIO` and the loop ends.
fn pump(
    mut poll: Poll,
    mut reader: File,
    terminal: &Arc<Mutex<Terminal>>,
    stop: &AtomicBool,
    writer: &Mutex<File>,
) {
    let fd = reader.as_raw_fd();
    if poll
        .registry()
        .register(&mut SourceFd(&fd), DATA, Interest::READABLE)
        .is_err()
    {
        return;
    }

    let mut events = Events::with_capacity(8);
    let mut buffer = [0u8; 4096];
    loop {
        if poll.poll(&mut events, None).is_err() {
            return; // poller failure — give up rather than spin
        }
        if stop.load(Ordering::Acquire) {
            return; // woken by Drop
        }
        // Drain everything currently available, then go back to sleep on the poll.
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return, // EOF — the child closed the PTY
                Ok(n) => lock(terminal).feed(&buffer[..n]),
                Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(ref error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return, // EIO etc. — the child is gone
            }
        }
        // Answer any terminal queries the child made while we fed it (cursor-position report, device
        // attributes, …) so query-driven programs (nu/reedline, vim, …) don't block on the reply.
        let responses = lock(terminal).take_responses();
        if !responses.is_empty() {
            let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
            if writer.write_all(&responses).is_ok() {
                let _ = writer.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PtyTerminal;
    use penumbra::{Buffer, Theme};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn live_shell_output_reaches_the_grid() {
        // Run a command that prints and exits; pump its output through a real PTY.
        let Ok(pty) = PtyTerminal::spawn_command("/bin/sh", &["-c", "printf hello"], 20, 3) else {
            eprintln!("skipping: no PTY available in this environment");
            return;
        };

        // Wait for the child to exit and its output to be drained (reader thread ends).
        for _ in 0..500 {
            if !pty.is_running() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let theme = Theme::steelbore();
        let mut surface = Buffer::new(20, 3, theme.base_style());
        pty.render(&mut surface, &theme);
        let row0: String = (0..20)
            .filter_map(|col| surface.cell(col, 0).map(|cell| cell.symbol))
            .collect();
        assert!(row0.starts_with("hello"), "grid row 0 was {row0:?}");
    }
}
