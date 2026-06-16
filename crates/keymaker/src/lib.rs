// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Keymaker — Majestic's keymap and input-profile subsystem (PRD #1 §5.2.1).
//!
//! A [`Keymap`] is an immutable prefix tree from key sequences to named [`Command`]s;
//! [`Keymap::bind`] returns a new value that structurally shares untouched subtrees, so
//! rebinding at runtime never disturbs dispatch already in flight. A [`Dispatcher`] resolves
//! keys against a stack of layers (buffer-local → minor-mode → major-mode → global), handling
//! multi-key prefixes and reporting unbound chords so the editor can self-insert.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! # Examples
//! ```
//! use keymaker::{cua, Dispatcher, KeyPress, Resolution, Command};
//!
//! let mut dispatcher = Dispatcher::new(vec![cua()]); // global layer only
//! assert_eq!(
//!     dispatcher.feed(KeyPress::ctrl('s')),
//!     Resolution::Command(Command::new("save"))
//! );
//! // A printable key is unbound in CUA, so the editor self-inserts it.
//! assert!(matches!(dispatcher.feed(KeyPress::char('h')), Resolution::Unbound(_)));
//! ```
//!
//! # Status (M0)
//! The CUA profile (Standard §8), persistent keymaps, and layered dispatch are implemented;
//! the Emacs / Vim / Spacemacs profiles, the first-run selector, and which-key hints land in
//! M2. Pure `std` — no terminal backend dependency; `crossterm` key events translate into
//! [`KeyPress`] when that layer is wired.

mod dispatch;
mod key;
mod keymap;
mod profiles;

#[doc(inline)]
pub use dispatch::{Dispatcher, Resolution};
#[doc(inline)]
pub use key::{KeyCode, KeyPress, Mods};
#[doc(inline)]
pub use keymap::{Command, Keymap, Lookup};
#[doc(inline)]
pub use profiles::cua;
