// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic — the `mj` binary entry point.
//!
//! TUI-first terminal, editor, and coding agent — Concept #1 (Rust + Steel). A thin launcher: the
//! editor lives in the [`majestic`] library, shared with the `mj-nova` GPU front end (M4). See the
//! workspace `MAJESTIC.md` for architecture and the milestone roadmap.

use std::process::ExitCode;

fn main() -> ExitCode {
    majestic::run()
}
