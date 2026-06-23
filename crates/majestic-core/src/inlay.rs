// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Inlay hints — inline virtual annotations (type/parameter labels) the language server suggests
//! (LSP `textDocument/inlayHint`).
//!
//! A hint is virtual text rendered *between* real characters at a byte position; it does not exist
//! in the buffer. The renderer draws each line's hints interleaved with its text (in a muted style),
//! and the cursor's display column accounts for any hints before it on the line so it still lands
//! under the right cell. Hints are transient — the host re-requests them as the buffer changes.

/// A single inlay hint: virtual text shown just before [`Self::byte`] in the buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayHint {
    /// The byte offset the hint renders immediately before (a char boundary, or a line end).
    pub byte: usize,
    /// The display text, already including any padding space the server asked for on either side.
    pub text: String,
}

impl InlayHint {
    /// Creates a hint rendered just before `byte`.
    #[must_use]
    pub fn new(byte: usize, text: impl Into<String>) -> Self {
        Self {
            byte,
            text: text.into(),
        }
    }
}
