// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic-core — composes the engine crates into the editing model (PRD #1 §6).
//!
//! [`Buffer`] is the document: Stratum's rope under a branching undo tree, plus a
//! char-boundary cursor, selection, and file I/O. [`Editor`] wires a buffer to the Keymaker
//! [`Dispatcher`](keymaker::Dispatcher) (CUA keymap), a clipboard, and a viewport, turning
//! keys into commands and rendering the result into a Penumbra buffer.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! # Examples
//! ```
//! use majestic_core::Editor;
//! use keymaker::{KeyCode, KeyPress};
//!
//! let mut editor = Editor::new();
//! editor.handle_key(KeyPress::char('h')); // unbound printable -> self-insert
//! editor.handle_key(KeyPress::char('i'));
//! editor.handle_key(KeyPress::key(KeyCode::Left));
//! editor.handle_key(KeyPress::char('!'));
//! assert_eq!(editor.buffer().text(), "h!i");
//! editor.handle_key(KeyPress::ctrl('z')); // undo the '!'
//! assert_eq!(editor.buffer().text(), "hi");
//! ```
//!
//! # Status (M0)
//! Single buffer/window editing with the CUA command set, undo/redo, selection/clipboard,
//! file open/save, and framebuffer rendering. Windows/splits, modes, sessions, the crash
//! journal + recovery, and the interactive `crossterm` loop land in the following steps.

mod buffer;
mod completion;
mod diagnostic;
mod editor;
mod files;
mod finder;
mod fuzzy;
mod git;
mod hover;
mod info;
mod prompt;
mod references;
mod rename;
mod selector;
mod session;
mod signature;
mod symbols;
mod syntax;
#[cfg(feature = "syntect-highlighting")]
mod syntect_hl;
mod whichkey;
mod workspace;

#[doc(inline)]
pub use buffer::Buffer;
#[doc(inline)]
pub use completion::{Completion, CompletionItem};
#[doc(inline)]
pub use diagnostic::{Diagnostic, Severity};
#[doc(inline)]
pub use editor::{EditMode, Editor};
#[doc(inline)]
pub use files::FileTree;
#[doc(inline)]
pub use finder::{Action, Finder, HelpOverlay};
#[doc(inline)]
pub use hover::Hover;
#[doc(inline)]
pub use info::{InfoDocument, InfoReader};
#[doc(inline)]
pub use prompt::Prompt;
#[doc(inline)]
pub use references::{Reference, References};
#[doc(inline)]
pub use rename::RenameEdit;
#[doc(inline)]
pub use selector::ProfileSelector;
#[doc(inline)]
pub use session::{LayoutNode, PaneState, Session};
#[doc(inline)]
pub use signature::SignatureHelp;
#[doc(inline)]
pub use symbols::{Symbol, Symbols};
#[doc(inline)]
pub use whichkey::WhichKey;
#[doc(inline)]
pub use workspace::{Split, Workspace};
