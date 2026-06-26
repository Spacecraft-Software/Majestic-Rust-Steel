// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! `mj-nova` — Nova's GPU window front end (M4).
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! M4.4: runs the **live** Majestic editor — the same [`majestic::App`] the TTY `mj` drives — in a
//! wgpu window, rendered through the Nova glyph pipeline. Open files as arguments (or none for a
//! scratch buffer); the editor reflows to the window. Only built with the `gpu` feature
//! (`cargo run -p nova --features gpu --bin mj-nova [FILE...]`); the TTY `mj` is unaffected.

use majestic::App;
use majestic_core::{Buffer, Editor, Workspace};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let mut editors = Vec::with_capacity(paths.len());
    for path in &paths {
        match Buffer::open(path) {
            Ok(buffer) => editors.push(Editor::with_buffer(buffer)),
            Err(error) => {
                eprintln!("mj-nova: cannot open {path}: {error}");
                return Ok(()); // a bad path aborts startup, like `mj`
            }
        }
    }
    if editors.is_empty() {
        editors.push(Editor::new()); // a scratch buffer when no files are given
    }
    // Honour the user's configuration — the keymap profile (Emacs/Vim/CUA/Spacemacs) and tab width —
    // just as the TTY `mj` does, so the GUI is the same editor (M4 polish). Fail-soft: a bad manifest
    // keeps the defaults.
    let mut workspace = Workspace::from_editors(editors);
    majestic::load_config(&mut workspace);
    let app = App::new(workspace);
    nova::run_editor(app)?;
    Ok(())
}
