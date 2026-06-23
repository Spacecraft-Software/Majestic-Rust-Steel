// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Editor-facing LSP rename edits (PRD #1 §6.9).
//!
//! One text replacement within a file, in LSP `(line, character)` coordinates. The LSP layer reduces
//! the server's `WorkspaceEdit` to a flat list of these (each carrying its own `path`, so edits may
//! span many files); the host groups them by file, reveals each file, and applies its edits
//! back-to-front (so earlier offsets stay valid). Pure data — no LSP dependency in the core.

use std::path::PathBuf;

/// A single text replacement produced by a rename, in LSP coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameEdit {
    /// The file this edit applies to.
    pub path: PathBuf,
    /// Zero-based start line of the range to replace.
    pub start_line: u32,
    /// Zero-based start character of the range to replace.
    pub start_character: u32,
    /// Zero-based end line of the range to replace.
    pub end_line: u32,
    /// Zero-based end character of the range to replace.
    pub end_character: u32,
    /// The text to put in place of the range (the new name).
    pub new_text: String,
}
