// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Glyph rasterisation for Nova (M4.3a): a cell's character → an alpha-coverage bitmap.
//!
//! [`GlyphRaster`] wraps `cosmic-text` (`FontSystem` + `SwashCache`) over a single **vendored**
//! monospace font (Adwaita Mono, OFL-1.1 — offline-first, never a system or fetched font), and turns
//! a `char` into a [`RasterGlyph`]: an 8-bit alpha bitmap plus its placement and advance. The GPU
//! glyph atlas + textured-quad pass that *upload* these and draw them land in M4.3b; this module is
//! the pure-CPU, unit-testable rasterisation core (and where [`crate::CellMetrics`] gets real values
//! from the font instead of the hard-coded 8×16 M4.2 used).

use cosmic_text::{
    fontdb, Attrs, Buffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent,
};

use crate::CellMetrics;

/// The vendored monospace font (Adwaita Mono, built from Iosevka — OFL-1.1; see the `.license`
/// sidecar). Embedded so Nova never depends on a system font (Standard §7 PFA / offline-first).
const FONT: &[u8] = include_bytes!("../assets/fonts/AdwaitaMono-Regular.ttf");

/// The font family name, used to select the vendored font when shaping.
const FAMILY: &str = "Adwaita Mono";

/// The line-height multiple of the font size, giving the cell height (a little leading above the
/// glyph box, the terminal convention).
const LINE_HEIGHT_RATIO: f32 = 1.25;

/// A rasterised glyph: an 8-bit alpha-coverage bitmap (`width × height`, one byte per pixel) plus its
/// placement relative to the pen and its advance, all in pixels at the raster's font size.
#[derive(Clone, Debug)]
pub struct RasterGlyph {
    /// X offset of the bitmap's left edge from the pen position (may be negative).
    pub left: i32,
    /// Y offset of the bitmap's top edge above the baseline (positive is up).
    pub top: i32,
    /// Bitmap width in pixels.
    pub width: u32,
    /// Bitmap height in pixels.
    pub height: u32,
    /// The glyph's horizontal advance in pixels (the monospace cell width).
    pub advance: f32,
    /// `width × height` bytes of alpha coverage, row-major.
    pub coverage: Vec<u8>,
}

/// Rasterises characters of the vendored monospace font at a fixed pixel size. Cheap to call
/// repeatedly (the GPU atlas in M4.3b caches the results); holds the `cosmic-text` font + glyph caches.
#[derive(Debug)]
pub struct GlyphRaster {
    font_system: FontSystem,
    swash_cache: SwashCache,
    font_size: f32,
    line_height: f32,
}

impl GlyphRaster {
    /// A rasteriser for the vendored monospace font at `font_size` pixels.
    #[must_use]
    pub fn new(font_size: f32) -> Self {
        let mut db = fontdb::Database::new();
        db.load_font_data(FONT.to_vec());
        Self {
            font_system: FontSystem::new_with_locale_and_db("en-US".to_owned(), db),
            swash_cache: SwashCache::new(),
            font_size,
            line_height: (font_size * LINE_HEIGHT_RATIO).round(),
        }
    }

    /// The pixel-exact cell box for this font: the monospace advance × the line height.
    pub fn cell_metrics(&mut self) -> CellMetrics {
        let advance = self.shape('M').map_or(self.font_size * 0.6, |(advance, _)| advance);
        CellMetrics::new(advance, self.line_height)
    }

    /// Rasterises `ch` to an alpha bitmap + placement, or `None` if the font has no (mask) glyph for
    /// it (e.g. an unmapped codepoint, or a colour/emoji glyph — handled later).
    pub fn rasterize(&mut self, ch: char) -> Option<RasterGlyph> {
        let (advance, cache_key) = self.shape(ch)?;
        let image = self.swash_cache.get_image(&mut self.font_system, cache_key).as_ref()?;
        if !matches!(image.content, SwashContent::Mask) {
            return None; // colour (emoji) glyphs are an M4.3+ follow-up; M4.3 handles mask glyphs
        }
        Some(RasterGlyph {
            left: image.placement.left,
            top: image.placement.top,
            width: image.placement.width,
            height: image.placement.height,
            advance,
            coverage: image.data.clone(),
        })
    }

    /// Shapes a single character and returns its `(advance, cache_key)`, or `None` if it produced no
    /// glyph. The advance is the monospace cell width; the cache key drives [`SwashCache`].
    fn shape(&mut self, ch: char) -> Option<(f32, CacheKey)> {
        let mut buffer =
            Buffer::new(&mut self.font_system, Metrics::new(self.font_size, self.line_height));
        // `borrowed` holds `&mut buffer` + `&mut self.font_system`; NLL ends those borrows at its last
        // use (`shape_until_scroll`), so `buffer` is free to read immediately below — no scoping block.
        let mut borrowed = buffer.borrow_with(&mut self.font_system);
        borrowed.set_text(
            ch.encode_utf8(&mut [0u8; 4]),
            &Attrs::new().family(Family::Name(FAMILY)),
            Shaping::Advanced,
            None,
        );
        borrowed.shape_until_scroll(false);
        let glyph = buffer.layout_runs().next()?.glyphs.first()?;
        Some((glyph.w, glyph.physical((0.0, 0.0), 1.0).cache_key))
    }
}

#[cfg(test)]
mod tests {
    use super::GlyphRaster;

    #[test]
    fn rasterises_a_glyph_to_an_alpha_bitmap_with_ink() {
        let mut raster = GlyphRaster::new(16.0);
        let glyph = raster.rasterize('M').expect("the font has an 'M'");
        assert!(glyph.width > 0 && glyph.height > 0, "'M' has a non-empty box");
        assert_eq!(
            glyph.coverage.len(),
            glyph.width as usize * glyph.height as usize,
            "one alpha byte per pixel"
        );
        assert!(glyph.coverage.iter().any(|&a| a > 0), "'M' has some ink");
        assert!(glyph.advance > 0.0, "and a positive advance");
    }

    #[test]
    fn cell_metrics_are_a_sane_monospace_box_from_the_font() {
        let mut raster = GlyphRaster::new(16.0);
        let metrics = raster.cell_metrics();
        assert!(metrics.width > 0.0, "the cell has a width (the advance)");
        assert!(metrics.height >= 16.0, "the cell is at least the font size tall");
    }

    #[test]
    fn every_ascii_letter_and_digit_rasterises() {
        let mut raster = GlyphRaster::new(16.0);
        for ch in ('A'..='Z').chain('a'..='z').chain('0'..='9') {
            assert!(raster.rasterize(ch).is_some(), "the font covers {ch}");
        }
    }
}
