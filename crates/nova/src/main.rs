// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! `mj-nova` — Nova's GPU window front end (M4).
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//!
//! M4.3 milestone: opens a wgpu window and paints a static editor mock — coloured regions (M4.2) plus
//! real **text** rendered through the cosmic-text glyph atlas (M4.3b). The live editor frame is M4.4.
//! Only built with the `gpu` feature (`cargo run -p nova --features gpu --bin mj-nova`); the TTY `mj`
//! is unaffected.

use penumbra::{Buffer, Cell, Rgb, Style, Theme};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let theme = Theme::steelbore();
    let frame = demo_frame(&theme);
    // Cell metrics come from the bundled font (M4.3), so the grid matches the rasterised glyphs.
    let metrics = nova::GlyphRaster::new(nova::FONT_SIZE).cell_metrics();
    let scene = nova::build_scene(&frame, metrics);
    let bg = theme.background;
    let clear = [
        f64::from(bg.r) / 255.0,
        f64::from(bg.g) / 255.0,
        f64::from(bg.b) / 255.0,
        1.0,
    ];
    nova::run(scene, clear)?;
    Ok(())
}

/// A static 120 × 36 editor mock on Void Navy: an explorer sidebar, a code editor, an Architect panel,
/// and a status bar — with real text — so the window proves the glyph atlas + textured-quad pass.
fn demo_frame(theme: &Theme) -> Buffer {
    let (cols, rows) = (120_u16, 36_u16);
    let mut frame = Buffer::new(cols, rows, Style::new(theme.foreground, theme.background));
    let text = Style::new(theme.foreground, theme.background); // Molten Amber on Void Navy
    let dim = Style::new(theme.accent, theme.background); // Steel Blue (labels / comments)
    let info = Style::new(theme.info, theme.background); // Liquid Coolant (accents)

    // Vertical dividers: explorer | editor | Architect panel.
    paint(&mut frame, 23, 24, 0, rows - 1, theme.accent, theme.background);
    paint(&mut frame, cols - 36, cols - 35, 0, rows - 1, theme.accent, theme.background);

    // Explorer sidebar.
    frame.set_str(1, 0, "EXPLORER", dim);
    frame.set_str(1, 2, "v majestic-rust", info);
    let tree = [
        " Cargo.toml",
        " README.md",
        " v crates/nova/src",
        "     atlas.rs",
        "     raster.rs",
        "     renderer.rs",
        "     window.rs",
        " v mj/src",
        "     main.rs",
        "     tui.rs",
    ];
    write_lines(&mut frame, 1, 4, &tree, text);

    // Code editor, with a line-number gutter.
    let code = [
        "// nova/src/renderer.rs — the GPU side of Nova",
        "",
        "pub fn render(&mut self, scene: &Scene) {",
        "    let glyphs = self.glyph_instances(scene);",
        "    // backgrounds first, then glyphs over them",
        "    pass.set_pipeline(&self.glyph_pipeline);",
        "    pass.set_bind_group(1, self.atlas.bind(), &[]);",
        "    pass.draw(0..6, 0..glyph_count);",
        "}",
    ];
    for (i, _) in code.iter().enumerate() {
        let row = 3 + u16::try_from(i).unwrap_or(0);
        let line_no = i + 1;
        frame.set_str(25, row, &format!("{line_no:>3}"), dim);
    }
    write_lines(&mut frame, 29, 3, &code, text);
    // A cursor cell on the open-brace line.
    paint(&mut frame, 64, 65, 5, 6, theme.info, theme.background);

    // Architect panel.
    frame.set_str(cols - 34, 0, "ARCHITECT", dim);
    frame.set_str(cols - 34, 2, "> render the glyphs", info);
    let reply = [
        "Done. M4.3b draws the",
        "Scene's glyphs as textured",
        "quads sampling a cosmic-text",
        "atlas, alpha-blended over",
        "the cell backgrounds.",
    ];
    write_lines(&mut frame, cols - 34, 4, &reply, text);

    // Status bar across the bottom.
    paint(&mut frame, 0, cols, rows - 1, rows, theme.accent, theme.background);
    let status = Style::new(theme.background, theme.accent);
    frame.set_str(
        1,
        rows - 1,
        " mj   Majestic — Nova (M4.3)    rust    Ln 9, Col 2    UTF-8 ",
        status,
    );

    frame
}

/// Writes `lines` top-to-bottom starting at `(x, y0)`.
fn write_lines(frame: &mut Buffer, x: u16, y0: u16, lines: &[&str], style: Style) {
    for (i, line) in lines.iter().enumerate() {
        frame.set_str(x, y0 + u16::try_from(i).unwrap_or(0), line, style);
    }
}

/// Fills the cell rectangle `[x0, x1) × [y0, y1)` with background colour `bg` (foreground `fg`).
fn paint(frame: &mut Buffer, x0: u16, x1: u16, y0: u16, y1: u16, bg: Rgb, fg: Rgb) {
    for y in y0..y1 {
        for x in x0..x1 {
            frame.set(x, y, Cell::new(' ', Style::new(fg, bg)));
        }
    }
}
