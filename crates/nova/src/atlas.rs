// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The glyph atlas for Nova (M4.3b): rasterised glyphs packed into one GPU texture.
//!
//! [`GlyphAtlas`] lazily rasterises each character (via [`GlyphRaster`](crate::GlyphRaster)), packs
//! its alpha bitmap into a single `R8Unorm` texture with a simple shelf packer, and caches the
//! resulting [`AtlasEntry`] (UVs + placement) per `char`. The renderer turns each
//! [`Scene`](crate::Scene) glyph into a textured quad sampling this atlas (M4.3b's second pass).

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};

use crate::raster::GlyphRaster;
use crate::CellMetrics;

/// The square atlas texture's side in pixels — room for the ASCII set (and then some) at editor sizes.
pub const ATLAS_SIZE: u32 = 1024;

/// One pixel of padding between packed glyphs, so nearest-sampling never bleeds a neighbour in.
const PADDING: u32 = 1;

/// One glyph's place in the atlas: its texture UVs plus its pixel placement relative to the pen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasEntry {
    /// Top-left atlas UV (0..1).
    pub uv_min: [f32; 2],
    /// Bottom-right atlas UV (0..1).
    pub uv_max: [f32; 2],
    /// X offset of the bitmap from the pen, in pixels.
    pub left: f32,
    /// Y offset of the bitmap's top above the baseline, in pixels.
    pub top: f32,
    /// Bitmap width in pixels.
    pub width: f32,
    /// Bitmap height in pixels.
    pub height: f32,
}

/// A per-instance textured quad for the glyph pass: a pixel rect, the atlas UV rect, and the colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct GlyphInstance {
    /// Top-left pixel position.
    pub pos: [f32; 2],
    /// Size in pixels.
    pub size: [f32; 2],
    /// Top-left atlas UV.
    pub uv_min: [f32; 2],
    /// Bottom-right atlas UV.
    pub uv_max: [f32; 2],
    /// sRGB-normalised glyph colour.
    pub color: [f32; 4],
}

/// A trivial shelf packer: glyphs are laid left-to-right in rows of the current row's height, wrapping
/// to a new shelf when the row is full. Returns `None` when the atlas is full (a glyph is then skipped).
#[derive(Clone, Copy, Debug)]
struct ShelfPacker {
    size: u32,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
}

impl ShelfPacker {
    const fn new(size: u32) -> Self {
        Self {
            size,
            cursor_x: 0,
            cursor_y: 0,
            row_height: 0,
        }
    }

    /// Reserves a `width × height` cell and returns its top-left, or `None` if the atlas is full.
    fn place(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if self.cursor_x + width > self.size {
            // Wrap to a new shelf below the tallest glyph on the current row.
            self.cursor_y += self.row_height + PADDING;
            self.cursor_x = 0;
            self.row_height = 0;
        }
        if self.cursor_x + width > self.size || self.cursor_y + height > self.size {
            return None; // does not fit even on a fresh shelf — atlas full
        }
        let position = (self.cursor_x, self.cursor_y);
        self.cursor_x += width + PADDING;
        self.row_height = self.row_height.max(height);
        Some(position)
    }
}

/// A GPU glyph atlas: a single `R8Unorm` texture, its bind group, and a per-`char` cache of packed
/// glyphs. The font is rasterised on demand by [`GlyphRaster`]; the baseline ([`Self::ascent`]) places
/// each glyph within its cell.
#[derive(Debug)]
pub struct GlyphAtlas {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    packer: ShelfPacker,
    cache: HashMap<(char, bool), Option<AtlasEntry>>,
    raster: GlyphRaster,
    ascent: f32,
    cell_metrics: CellMetrics,
}

impl GlyphAtlas {
    /// Builds an empty atlas texture + bind group (using `layout`) and a rasteriser at `font_size`.
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        font_size: f32,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nova-glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nova-glyph-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nova-glyph-bind"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let mut raster = GlyphRaster::new(font_size);
        let ascent = raster.ascent();
        let cell_metrics = raster.cell_metrics();
        Self {
            texture,
            bind_group,
            packer: ShelfPacker::new(ATLAS_SIZE),
            cache: HashMap::new(),
            raster,
            ascent,
            cell_metrics,
        }
    }

    /// The baseline offset (pixels from the top of a cell), for placing glyphs within their cell.
    #[must_use]
    pub fn ascent(&self) -> f32 {
        self.ascent
    }

    /// The pixel-exact cell box (advance × line height) for this atlas's font — what a front end
    /// divides the window by to get the cell grid, and feeds to `build_scene`.
    #[must_use]
    pub fn cell_metrics(&self) -> CellMetrics {
        self.cell_metrics
    }

    /// The atlas texture's bind group (group 1 of the glyph pipeline).
    #[must_use]
    pub fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    /// Returns the atlas entry for `ch`, rasterising + uploading it on first use. When `icon`, `ch` is
    /// a Material icon codepoint drawn from the icon font (M4.6). `None` if the font has no glyph for it
    /// or the atlas is full (the glyph is then simply not drawn). Cached per `(char, icon)`.
    pub fn entry(&mut self, queue: &wgpu::Queue, ch: char, icon: bool) -> Option<AtlasEntry> {
        if let Some(cached) = self.cache.get(&(ch, icon)) {
            return *cached;
        }
        let entry = self.rasterise_and_pack(queue, ch, icon);
        self.cache.insert((ch, icon), entry);
        entry
    }

    /// Rasterises `ch` (from the icon font when `icon`), packs it into the atlas texture, and returns
    /// its [`AtlasEntry`].
    fn rasterise_and_pack(&mut self, queue: &wgpu::Queue, ch: char, icon: bool) -> Option<AtlasEntry> {
        let glyph = self.raster.rasterize(ch, icon)?;
        if glyph.width == 0 || glyph.height == 0 {
            return None; // whitespace etc. — an advance but nothing to draw
        }
        let (x, y) = self.packer.place(glyph.width, glyph.height)?;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &glyph.coverage,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(glyph.width),
                rows_per_image: Some(glyph.height),
            },
            wgpu::Extent3d {
                width: glyph.width,
                height: glyph.height,
                depth_or_array_layers: 1,
            },
        );
        #[expect(
            clippy::cast_precision_loss,
            reason = "glyph bitmap dimensions/offsets are small, exactly representable in f32"
        )]
        let entry = AtlasEntry {
            uv_min: [x as f32 / ATLAS_SIZE as f32, y as f32 / ATLAS_SIZE as f32],
            uv_max: [
                (x + glyph.width) as f32 / ATLAS_SIZE as f32,
                (y + glyph.height) as f32 / ATLAS_SIZE as f32,
            ],
            left: glyph.left as f32,
            top: glyph.top as f32,
            width: glyph.width as f32,
            height: glyph.height as f32,
        };
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::ShelfPacker;

    #[test]
    fn the_shelf_packer_lays_glyphs_left_to_right_then_wraps() {
        let mut packer = ShelfPacker::new(10);
        // Two 4×3 glyphs fit on the first shelf at x=0 and x=5 (4 + 1 padding).
        assert_eq!(packer.place(4, 3), Some((0, 0)));
        assert_eq!(packer.place(4, 3), Some((5, 0)));
        // A third 4-wide glyph overflows the 10-wide row → wraps to the next shelf (y = 3 + 1 padding).
        assert_eq!(packer.place(4, 2), Some((0, 4)));
    }

    #[test]
    fn the_shelf_packer_reports_full() {
        let mut packer = ShelfPacker::new(4);
        assert_eq!(packer.place(4, 4), Some((0, 0)));
        // The next shelf would start at y=5, past the 4-tall atlas → full.
        assert_eq!(packer.place(4, 4), None);
    }
}
