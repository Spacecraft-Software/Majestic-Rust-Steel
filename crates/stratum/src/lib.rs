// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Stratum — Majestic's text core.
//!
//! Stratum is the persistent, snapshot-capable document model behind Majestic: a
//! copy-on-write rope with `O(log n)` edits, cheap immutable snapshots, position anchors
//! that survive concurrent edits, interval-tagged spans, a branching undo tree, and a
//! crash-safe edit journal (PRD #1 §6.3). It is designed as a reusable library crate —
//! Concept #3 (RMS) and Concept #4 (BEAM) embed this exact engine.
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! # Status (M0)
//! The [`Rope`] and its [`Summary`]/[`Point`] coordinate machinery are implemented and
//! property-tested. Anchors, spans, the branching undo tree, and the journal land in the
//! subsequent M0 steps and will appear here as additional modules.
//!
//! # Examples
//! ```
//! use stratum::{Point, Rope};
//!
//! let doc = Rope::from("fn main() {}\n");
//! assert_eq!(doc.len_lines(), 2);
//! assert_eq!(doc.byte_to_point(3), Point::new(0, 3));
//!
//! // Edits are copy-on-write: the snapshot is unaffected.
//! let snapshot = doc.snapshot();
//! let doc = doc.insert(0, "// hi\n");
//! assert_eq!(snapshot.line(0), "fn main() {}");
//! assert_eq!(doc.line(0), "// hi");
//! ```

mod rope;
mod summary;

#[doc(inline)]
pub use rope::{Chunks, Rope};
#[doc(inline)]
pub use summary::{Point, Summary};
