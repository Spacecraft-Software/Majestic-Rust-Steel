// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic-term — the integrated terminal, wrapping `alacritty_terminal` (PRD #1 §6.6).
//!
//! [`Terminal`] embeds `alacritty_terminal`'s VT engine: push child-program output through
//! [`Terminal::feed`] and render the resulting cell grid — characters, xterm/truecolor
//! colors, and attributes — into a Penumbra [`Buffer`] via [`Terminal::render`]. Embedding
//! `alacritty_terminal` (rather than writing a VT parser) is what makes the terminal correct
//! across the long tail of escape sequences; the upstream is credited in `CREDITS.md`
//! (Apache-2.0, §4.2 / §13.3).
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! # Examples
//! ```
//! use majestic_term::Terminal;
//! use penumbra::{Buffer, Theme};
//!
//! let theme = Theme::steelbore();
//! let mut terminal = Terminal::new(20, 3);
//! terminal.feed(b"hi\r\nthere");
//!
//! let mut surface = Buffer::new(20, 3, theme.base_style());
//! terminal.render(&mut surface, &theme);
//! assert_eq!(surface.cell(0, 0).unwrap().symbol, 'h');
//! assert_eq!(surface.cell(0, 1).unwrap().symbol, 't');
//! ```
//!
//! # Status (M1)
//! The emulation core ([`Terminal`]: feed / resize / render with color + attribute
//! resolution) and a live [`PtyTerminal`] (spawns `$SHELL` over a PTY, pumps its output in on
//! a background reader thread, writes keystrokes back) are implemented and tested. Remaining:
//! wiring the terminal into the editor's window tree, copy mode, OSC 7 cwd tracking, a visible
//! cursor, and a `mio`/Morpheus-polled reader (the current one polls on a short sleep). Cells
//! are one column wide (wide-char handling lands with Penumbra's `unicode-width` support).

mod color;
mod pty;
mod terminal;

#[doc(inline)]
pub use pty::PtyTerminal;
#[doc(inline)]
pub use terminal::Terminal;
