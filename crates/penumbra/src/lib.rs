// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Penumbra — Majestic's TTY renderer and the product's primary interface (PRD #1 §6.5).
//!
//! Penumbra draws the whole logical frame immediate-mode into a [`Buffer`], diffs it against
//! the previously displayed frame, and emits only the changed cells as minimal VT escapes
//! (the msedit framebuffer-diff discipline) — efficient on the bare Linux console and over
//! SSH alike. [`Screen`] manages the front/back buffers; [`render`] is the diff-and-emit
//! core. All colors flow through the [`Theme`] tokens (Standard §9.1), never bare hex.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! # Examples
//! ```
//! use penumbra::{Screen, Theme};
//!
//! let theme = Theme::steelbore();
//! let mut screen = Screen::new(20, 3, theme.base_style());
//! screen.back_mut().set_str(0, 0, "fn main() {}", theme.base_style());
//!
//! let mut out: Vec<u8> = Vec::new();
//! screen.present(&mut out).unwrap(); // emits the first frame as VT escapes
//! assert!(!out.is_empty());
//!
//! // Drawing the identical frame again emits nothing — only changes cost output.
//! screen.back_mut().set_str(0, 0, "fn main() {}", theme.base_style());
//! let mut out2: Vec<u8> = Vec::new();
//! screen.present(&mut out2).unwrap();
//! assert!(out2.is_empty());
//! ```
//!
//! # M0 substrate note
//! This is the pure-`std` framebuffer/diff/emit and theme core, fully testable offline (a VT
//! reconstructor replays the emitted bytes in the tests). The `crossterm` layer — raw mode,
//! terminal size, key/mouse decode (Kitty protocol), bracketed paste, OSC 52 clipboard —
//! and `ratatui` widgets attach on top of this core when those deps are vendored. Cells are
//! one column wide for now; `unicode-width` double-width/grapheme handling lands with it.

mod buffer;
mod layout;
mod render;
mod theme;

#[doc(inline)]
pub use buffer::{Buffer, Cell};
#[doc(inline)]
pub use layout::Rect;
#[doc(inline)]
pub use render::{render, Screen};
#[doc(inline)]
pub use theme::{Attrs, Rgb, Style, Theme};
