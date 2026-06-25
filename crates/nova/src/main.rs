// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! `mj-nova` — Nova's GPU window front end (M4).
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! M4.2 milestone: opens a wgpu window and paints the editor's cell *layout* as coloured rectangles
//! (a static demo frame — glyphs land in M4.3, a live editor in M4.4). Only built with the `gpu`
//! feature (`cargo run -p nova --features gpu --bin mj-nova`); the TTY `mj` is unaffected.

use penumbra::{Buffer, Cell, Rgb, Style, Theme};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let theme = Theme::steelbore();
    let frame = demo_frame(&theme);
    let scene = nova::build_scene(&frame, nova::CellMetrics::new(8.0, 16.0));
    let background = theme.background;
    let clear = [
        f64::from(background.r) / 255.0,
        f64::from(background.g) / 255.0,
        f64::from(background.b) / 255.0,
        1.0,
    ];
    nova::run(scene, clear)?;
    Ok(())
}

/// A static 120 × 36 frame that sketches the editor shell as coloured regions on Void Navy — a status
/// bar, an explorer divider, an agent-panel band, and a couple of markers — so the window proves the
/// quad pipeline lays cells out correctly before glyphs exist (M4.3).
fn demo_frame(theme: &Theme) -> Buffer {
    let (cols, rows) = (120_u16, 36_u16);
    let mut frame = Buffer::new(cols, rows, Style::new(theme.foreground, theme.background));
    // Status bar across the bottom row (Steel Blue), like the TTY status line.
    paint(&mut frame, 0, cols, rows - 1, rows, theme.accent, theme.background);
    // Explorer sidebar divider (a Steel Blue column at the left).
    paint(&mut frame, 23, 24, 0, rows - 1, theme.accent, theme.background);
    // Agent-panel divider on the right.
    paint(&mut frame, cols - 36, cols - 35, 0, rows - 1, theme.accent, theme.background);
    // A "selection" block in the editor (Liquid Coolant) and an error marker (Red Oxide).
    paint(&mut frame, 30, 52, 6, 9, theme.info, theme.foreground);
    paint(&mut frame, 30, 31, 12, 13, theme.error, theme.foreground);
    frame
}

/// Fills the cell rectangle `[x0, x1) × [y0, y1)` with background colour `bg` (foreground `fg`).
fn paint(frame: &mut Buffer, x0: u16, x1: u16, y0: u16, y1: u16, bg: Rgb, fg: Rgb) {
    for y in y0..y1 {
        for x in x0..x1 {
            frame.set(x, y, Cell::new(' ', Style::new(fg, bg)));
        }
    }
}
