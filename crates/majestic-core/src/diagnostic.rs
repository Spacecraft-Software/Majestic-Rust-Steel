// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Buffer [`Diagnostic`]s — the problems a language server reports for a document (PRD #1 §6.9).
//!
//! A diagnostic is stored as a **byte range** into the buffer (not an LSP line/character position),
//! so the renderer can underline the offending span without any rope lookups; the LSP layer
//! converts server positions to byte offsets when it sets them. The editor underlines each span in
//! its severity color and shows the cursor line's message in the status bar.

use std::ops::Range;

/// How serious a diagnostic is (mirrors the LSP `DiagnosticSeverity` values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// An error — the code will not build/run.
    Error,
    /// A warning — suspicious but not fatal.
    Warning,
    /// Informational.
    Information,
    /// A hint (e.g. an unused-import fade).
    Hint,
}

/// A single diagnostic reported for a buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The affected byte range within the buffer.
    pub range: Range<usize>,
    /// How serious it is.
    pub severity: Severity,
    /// The human-readable message.
    pub message: String,
}

impl Diagnostic {
    /// Creates a diagnostic over `range`.
    #[must_use]
    pub fn new(range: Range<usize>, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            range,
            severity,
            message: message.into(),
        }
    }

    /// Whether byte `offset` falls within this diagnostic's span. A zero-width span (`start ==
    /// end`, e.g. a missing token) still covers its single start position.
    #[must_use]
    pub fn covers(&self, offset: usize) -> bool {
        if self.range.start == self.range.end {
            offset == self.range.start
        } else {
            self.range.contains(&offset)
        }
    }
}
