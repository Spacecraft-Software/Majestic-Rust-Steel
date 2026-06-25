// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Majestic GPU renderer (wgpu + cosmic-text).
//!
//! Part of [Majestic](https://Majestic.SpacecraftSoftware.org/) — Concept #1 (Rust + Steel).
//! The GUI renderer; lands in M4. See `docs/nova.md` for the architecture and chunk ladder.
//!
//! # The parity core (M4.1)
//!
//! Nova draws the *same* editor the TTY does. The shared layout layer is the
//! [`penumbra::Buffer`] — a grid of styled cells that `mj`'s `App::render` fills immediate-mode.
//! Penumbra diffs that buffer and emits VT; Nova reads the same buffer and emits GPU draw calls.
//! Same buffer in → logically identical picture out (PRD-01 §6.5 renderer-parity rule).
//!
//! [`build_scene`] is that translation, kept **pure and GPU-free** so it is unit-testable without a
//! window: a [`Buffer`] becomes a [`Scene`] — one background [`Quad`] per visible cell plus one
//! [`Glyph`] per non-blank cell — preserving the editor's cell semantics (reverse swaps fg/bg,
//! double-width glyphs span two columns and skip their continuation cell, blanks draw no glyph). The
//! wgpu surface + `cosmic-text` atlas that consume a [`Scene`] land in M4.2 / M4.3 behind the `gpu`
//! feature, so this model stays in the cheap `--workspace` gate.

use penumbra::{char_width, Buffer, Rgb};

/// The pixel size of one terminal cell. A parameter for now; M4.3 derives it from the chosen
/// monospace font's `cosmic-text` metrics so the grid is pixel-exact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    /// Cell width in pixels.
    pub width: f32,
    /// Cell height in pixels.
    pub height: f32,
}

impl CellMetrics {
    /// Cell metrics of `width` × `height` pixels.
    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// A filled rectangle in pixel coordinates (origin top-left) — a cell's background. `color` is
/// sRGB-normalised `[r, g, b, a]` in `0.0..=1.0`; the surface colour space is applied at upload
/// (M4.2), not here (see `docs/nova.md` §6).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    /// Left edge in pixels.
    pub x: f32,
    /// Top edge in pixels.
    pub y: f32,
    /// Width in pixels (two cells wide for a double-width glyph's cell).
    pub width: f32,
    /// Height in pixels (one cell).
    pub height: f32,
    /// sRGB-normalised fill colour `[r, g, b, a]`.
    pub color: [f32; 4],
}

/// A glyph to draw, anchored at the top-left pixel of its cell. `color` is the resolved foreground
/// (sRGB-normalised); `bold`/`underline` carry the cell's attributes for the text pass (M4.3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    /// The character to rasterise.
    pub ch: char,
    /// Left edge of the cell in pixels.
    pub x: f32,
    /// Top edge of the cell in pixels.
    pub y: f32,
    /// sRGB-normalised glyph colour `[r, g, b, a]`.
    pub color: [f32; 4],
    /// Whether the cell is bold.
    pub bold: bool,
    /// Whether the cell is underlined.
    pub underline: bool,
}

/// A renderer-agnostic draw list for one frame: background [`Quad`]s and [`Glyph`] placements, in
/// row-major draw order. The GPU pipeline (M4.2) uploads the quads to an instanced-rect pass and the
/// glyphs to the `cosmic-text` atlas pass; the parity suite (M4.5) asserts this without a GPU.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    /// One background rectangle per visible cell.
    pub quads: Vec<Quad>,
    /// One glyph per non-blank cell.
    pub glyphs: Vec<Glyph>,
}

/// An sRGB colour as a normalised `[r, g, b, a]` (alpha `1.0`), each channel `byte / 255`.
fn normalized(rgb: Rgb) -> [f32; 4] {
    [
        f32::from(rgb.r) / 255.0,
        f32::from(rgb.g) / 255.0,
        f32::from(rgb.b) / 255.0,
        1.0,
    ]
}

/// Translates a cell [`Buffer`] into a [`Scene`] of background quads + glyph placements, at
/// `metrics` pixels per cell — the pure, GPU-free parity core (see the crate docs).
///
/// Cell semantics are preserved exactly: `attrs.reverse` swaps foreground and background; a
/// double-width glyph emits a two-cell-wide quad and its continuation cell is skipped; a blank
/// (space) cell emits a background quad but no glyph.
#[must_use]
pub fn build_scene(buffer: &Buffer, metrics: CellMetrics) -> Scene {
    let mut scene = Scene::default();
    for row in 0..buffer.height() {
        let mut col = 0;
        while col < buffer.width() {
            let Some(cell) = buffer.cell(col, row) else {
                col = col.saturating_add(1);
                continue;
            };
            // A continuation cell ('\0') is the trailing half of a double-width glyph to its left,
            // already covered by that glyph's two-cell-wide quad — nothing to draw here.
            if cell.symbol == '\0' {
                col = col.saturating_add(1);
                continue;
            }
            let cells_wide = char_width(cell.symbol).max(1);
            // Reverse video renders the foreground as the fill and the background as the ink.
            let (ink, fill) = if cell.style.attrs.reverse {
                (cell.style.bg, cell.style.fg)
            } else {
                (cell.style.fg, cell.style.bg)
            };
            let x = f32::from(col) * metrics.width;
            let y = f32::from(row) * metrics.height;
            scene.quads.push(Quad {
                x,
                y,
                width: f32::from(cells_wide) * metrics.width,
                height: metrics.height,
                color: normalized(fill),
            });
            if cell.symbol != ' ' {
                scene.glyphs.push(Glyph {
                    ch: cell.symbol,
                    x,
                    y,
                    color: normalized(ink),
                    bold: cell.style.attrs.bold,
                    underline: cell.style.attrs.underline,
                });
            }
            col = col.saturating_add(cells_wide);
        }
    }
    scene
}

#[cfg(test)]
mod tests {
    use super::{build_scene, normalized, CellMetrics, Glyph, Quad};
    use penumbra::{Buffer, Rgb, Style};

    const METRICS: CellMetrics = CellMetrics::new(8.0, 16.0);

    fn style(fg: Rgb, bg: Rgb) -> Style {
        Style::new(fg, bg)
    }

    #[test]
    fn a_blank_buffer_is_all_background_quads_and_no_glyphs() {
        let bg = Rgb { r: 0, g: 0, b: 0x27 }; // Void Navy
        let buffer = Buffer::new(3, 2, style(Rgb { r: 0xD9, g: 0x8E, b: 0x32 }, bg));
        let scene = build_scene(&buffer, METRICS);

        assert!(scene.glyphs.is_empty(), "blank cells draw no glyphs");
        assert_eq!(scene.quads.len(), 6, "one background quad per cell");
        // The cell at column 2, row 1 sits at (16, 16) and is the background colour.
        assert_eq!(
            scene.quads[5],
            Quad {
                x: 16.0,
                y: 16.0,
                width: 8.0,
                height: 16.0,
                color: normalized(bg),
            }
        );
    }

    #[test]
    fn a_character_cell_emits_a_glyph_at_its_pixel_origin_in_the_foreground() {
        let fg = Rgb { r: 0xD9, g: 0x8E, b: 0x32 };
        let bg = Rgb { r: 0, g: 0, b: 0x27 };
        let mut buffer = Buffer::new(4, 2, style(fg, bg));
        buffer.set_char(2, 1, 'X', style(fg, bg));
        let scene = build_scene(&buffer, METRICS);

        assert_eq!(
            scene.glyphs,
            vec![Glyph {
                ch: 'X',
                x: 16.0,
                y: 16.0,
                color: normalized(fg),
                bold: false,
                underline: false,
            }]
        );
    }

    #[test]
    fn reverse_video_swaps_the_glyph_and_background_colours() {
        let fg = Rgb { r: 0xD9, g: 0x8E, b: 0x32 };
        let bg = Rgb { r: 0, g: 0, b: 0x27 };
        let mut reversed = style(fg, bg);
        reversed.attrs.reverse = true;
        let mut buffer = Buffer::new(1, 1, style(fg, bg));
        buffer.set_char(0, 0, 'A', reversed);
        let scene = build_scene(&buffer, METRICS);

        // The fill is the foreground and the ink is the background — the inverse of normal. (Whole-
        // struct comparison so the float colours go through the derived `PartialEq`, not a bare `==`.)
        assert_eq!(
            scene.quads,
            vec![Quad {
                x: 0.0,
                y: 0.0,
                width: 8.0,
                height: 16.0,
                color: normalized(fg),
            }]
        );
        assert_eq!(
            scene.glyphs,
            vec![Glyph {
                ch: 'A',
                x: 0.0,
                y: 0.0,
                color: normalized(bg),
                bold: false,
                underline: false,
            }]
        );
    }

    #[test]
    fn a_double_width_glyph_spans_two_cells_and_skips_its_continuation() {
        let fg = Rgb { r: 0xD9, g: 0x8E, b: 0x32 };
        let bg = Rgb { r: 0, g: 0, b: 0x27 };
        let mut buffer = Buffer::new(2, 1, style(fg, bg));
        buffer.set_char(0, 0, '好', style(fg, bg)); // width 2 → continuation written at (1, 0)
        let scene = build_scene(&buffer, METRICS);

        // One quad, two cells wide; one glyph; the continuation cell produced nothing.
        assert_eq!(
            scene.quads,
            vec![Quad {
                x: 0.0,
                y: 0.0,
                width: 16.0,
                height: 16.0,
                color: normalized(bg),
            }]
        );
        assert_eq!(scene.glyphs.len(), 1);
        assert_eq!(scene.glyphs[0].ch, '好');
    }

    #[test]
    fn normalized_maps_the_byte_range_to_unit_floats() {
        // Compare exact bit patterns (an integer compare) so this isn't a bare float `==`. The
        // endpoints 0/255 map exactly to 0.0/1.0, so the bit patterns are identical.
        assert_eq!(
            normalized(Rgb { r: 0, g: 0, b: 0 }).map(f32::to_bits),
            [0.0_f32, 0.0, 0.0, 1.0].map(f32::to_bits)
        );
        assert_eq!(
            normalized(Rgb {
                r: 255,
                g: 255,
                b: 255
            })
            .map(f32::to_bits),
            [1.0_f32, 1.0, 1.0, 1.0].map(f32::to_bits)
        );
    }
}
