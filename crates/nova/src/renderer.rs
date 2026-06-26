// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The wgpu side of Nova: turn a [`Scene`](crate::Scene) into pixels.
//!
//! [`Gpu`] owns the device, queue, two pipelines, and the glyph atlas. [`Gpu::render`] clears the
//! target to the Steelbore background, draws one instanced quad per cell background (M4.2), then draws
//! the scene's glyphs as textured quads sampling the [`GlyphAtlas`](crate::atlas::GlyphAtlas) (M4.3b) —
//! both in one render pass, the glyphs alpha-blended over the backgrounds.
//!
//! No `unsafe` (§6.1): the surface is created from a borrowed window through wgpu's safe
//! `create_surface`, and `bytemuck` keeps the vertex-byte casts inside its own crate.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt as _;

use crate::atlas::{GlyphAtlas, GlyphInstance};
use crate::Scene;

/// The instanced-rectangle (background) shader: each instance is a pixel-space rect + colour.
const QUAD_SHADER: &str = r"
struct Screen { size: vec2<f32> }
@group(0) @binding(0) var<uniform> screen: Screen;

struct VsOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) color: vec4<f32>,
}

@vertex
fn vs(@location(0) corner: vec2<f32>,
      @location(1) pos: vec2<f32>,
      @location(2) size: vec2<f32>,
      @location(3) color: vec4<f32>) -> VsOut {
  let px = pos + corner * size;
  let ndc = vec2<f32>(px.x / screen.size.x * 2.0 - 1.0,
                      1.0 - px.y / screen.size.y * 2.0);
  var out: VsOut;
  out.clip = vec4<f32>(ndc, 0.0, 1.0);
  out.color = color;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> { return in.color; }
";

/// The glyph shader: a textured quad sampling the R8 glyph atlas; the alpha coverage modulates the
/// glyph's foreground colour (alpha-blended over the backgrounds).
const GLYPH_SHADER: &str = r"
struct Screen { size: vec2<f32> }
@group(0) @binding(0) var<uniform> screen: Screen;
@group(1) @binding(0) var atlas: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

struct VsOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
}

@vertex
fn vs(@location(0) corner: vec2<f32>,
      @location(1) pos: vec2<f32>,
      @location(2) size: vec2<f32>,
      @location(3) uv_min: vec2<f32>,
      @location(4) uv_max: vec2<f32>,
      @location(5) color: vec4<f32>) -> VsOut {
  let px = pos + corner * size;
  let ndc = vec2<f32>(px.x / screen.size.x * 2.0 - 1.0,
                      1.0 - px.y / screen.size.y * 2.0);
  var out: VsOut;
  out.clip = vec4<f32>(ndc, 0.0, 1.0);
  out.uv = mix(uv_min, uv_max, corner);
  out.color = color;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  let coverage = textureSample(atlas, atlas_sampler, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * coverage);
}
";

/// The six corners (two triangles) of a unit quad, expanded per-instance in the vertex shader.
const QUAD_CORNERS: [[f32; 2]; 6] = [
    [0.0, 0.0],
    [1.0, 0.0],
    [0.0, 1.0],
    [0.0, 1.0],
    [1.0, 0.0],
    [1.0, 1.0],
];

/// One instanced rectangle: a pixel-space top-left + size and an sRGB-normalised colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct QuadInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

/// The surface size, as a 16-byte-aligned uniform (`vec2` + padding).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct ScreenUniform {
    size: [f32; 2],
    _pad: [f32; 2],
}

/// A wgpu device + queue, the background + glyph pipelines, and the glyph atlas. Render a [`Scene`]
/// with [`Gpu::render`]; create one for a window with [`Gpu::for_surface`] or off-screen with
/// [`Gpu::headless`].
#[derive(Debug)]
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    quad_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    corners: wgpu::Buffer,
    screen: wgpu::Buffer,
    screen_bind: wgpu::BindGroup,
    atlas: GlyphAtlas,
}

impl Gpu {
    /// Builds the device, queue, both pipelines, and the glyph atlas targeting `format`.
    fn build(adapter: &wgpu::Adapter, format: wgpu::TextureFormat) -> Self {
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("nova-device"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .expect("the adapter must provide a device");

        let screen_layout = screen_bind_group_layout(&device);
        let atlas_layout = atlas_bind_group_layout(&device);
        let quad_pipeline = quad_pipeline(&device, &screen_layout, format);
        let glyph_pipeline = glyph_pipeline(&device, &screen_layout, &atlas_layout, format);

        let corners = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("nova-corners"),
            contents: bytemuck::cast_slice(&QUAD_CORNERS),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let screen = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nova-screen-uniform"),
            size: std::mem::size_of::<ScreenUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let screen_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nova-screen-bind"),
            layout: &screen_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen.as_entire_binding(),
            }],
        });
        let atlas = GlyphAtlas::new(&device, &atlas_layout, crate::FONT_SIZE);

        Self {
            device,
            queue,
            quad_pipeline,
            glyph_pipeline,
            corners,
            screen,
            screen_bind,
            atlas,
        }
    }

    /// Clears `view` to `clear` (sRGB-normalised `[r, g, b, a]`), draws `scene`'s background quads,
    /// then its glyphs (alpha-blended over them), sized to a `size`-pixel surface.
    pub fn render(&mut self, view: &wgpu::TextureView, size: (u32, u32), scene: &Scene, clear: [f64; 4]) {
        let (width, height) = size;
        #[expect(
            clippy::cast_precision_loss,
            reason = "surface dimensions are small, well within f32's exact-integer range"
        )]
        let screen = ScreenUniform {
            size: [width as f32, height as f32],
            _pad: [0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.screen, 0, bytemuck::cast_slice(&[screen]));

        let quads: Vec<QuadInstance> = scene
            .quads
            .iter()
            .map(|quad| QuadInstance {
                pos: [quad.x, quad.y],
                size: [quad.width, quad.height],
                color: quad.color,
            })
            .collect();
        let quad_count = u32::try_from(quads.len()).unwrap_or(u32::MAX);
        let quad_buf = vertex_buffer(&self.device, "nova-quads", &quads);

        let glyphs = self.glyph_instances(scene);
        let glyph_count = u32::try_from(glyphs.len()).unwrap_or(u32::MAX);
        let glyph_buf = (glyph_count > 0).then(|| vertex_buffer(&self.device, "nova-glyphs", &glyphs));

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nova-frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nova-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0],
                            g: clear[1],
                            b: clear[2],
                            a: clear[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if quad_count > 0 {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_bind_group(0, &self.screen_bind, &[]);
                pass.set_vertex_buffer(0, self.corners.slice(..));
                pass.set_vertex_buffer(1, quad_buf.slice(..));
                pass.draw(0..6, 0..quad_count);
            }
            if let Some(glyph_buf) = &glyph_buf {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.screen_bind, &[]);
                pass.set_bind_group(1, self.atlas.bind_group(), &[]);
                pass.set_vertex_buffer(0, self.corners.slice(..));
                pass.set_vertex_buffer(1, glyph_buf.slice(..));
                pass.draw(0..6, 0..glyph_count);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
    }

    /// Builds the per-glyph textured-quad instances for `scene`, rasterising + atlasing each glyph on
    /// first use and placing it on the cell's baseline (`pos = cell + (left, ascent - top)`).
    fn glyph_instances(&mut self, scene: &Scene) -> Vec<GlyphInstance> {
        let ascent = self.atlas.ascent();
        let mut instances = Vec::with_capacity(scene.glyphs.len());
        for glyph in &scene.glyphs {
            if let Some(entry) = self.atlas.entry(&self.queue, glyph.ch) {
                instances.push(GlyphInstance {
                    pos: [glyph.x + entry.left, glyph.y + ascent - entry.top],
                    size: [entry.width, entry.height],
                    uv_min: entry.uv_min,
                    uv_max: entry.uv_max,
                    color: glyph.color,
                });
            }
        }
        instances
    }
}

/// Uploads `instances` as a vertex buffer (a fresh per-frame buffer — caching is a later optimisation).
fn vertex_buffer<T: Pod>(device: &wgpu::Device, label: &str, instances: &[T]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(instances),
        usage: wgpu::BufferUsages::VERTEX,
    })
}

/// The bind-group layout for the screen-size uniform (group 0, both pipelines).
fn screen_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("nova-screen-layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    })
}

/// The bind-group layout for the glyph atlas: an R8 texture + a non-filtering sampler (group 1).
fn atlas_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("nova-atlas-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
        ],
    })
}

/// Builds the background instanced-rectangle pipeline.
fn quad_pipeline(
    device: &wgpu::Device,
    screen_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("nova-quad-shader"),
        source: wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("nova-quad-pipeline-layout"),
        bind_group_layouts: &[screen_layout],
        push_constant_ranges: &[],
    });
    render_pipeline(
        device,
        "nova-quad-pipeline",
        &layout,
        &shader,
        &[corner_layout(), quad_instance_layout()],
        format,
    )
}

/// Builds the glyph textured-quad pipeline (group 0 = screen, group 1 = atlas).
fn glyph_pipeline(
    device: &wgpu::Device,
    screen_layout: &wgpu::BindGroupLayout,
    atlas_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("nova-glyph-shader"),
        source: wgpu::ShaderSource::Wgsl(GLYPH_SHADER.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("nova-glyph-pipeline-layout"),
        bind_group_layouts: &[screen_layout, atlas_layout],
        push_constant_ranges: &[],
    });
    render_pipeline(
        device,
        "nova-glyph-pipeline",
        &layout,
        &shader,
        &[corner_layout(), glyph_instance_layout()],
        format,
    )
}

/// The shared render-pipeline builder (alpha blending, triangle list, a single colour target).
fn render_pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    buffers: &[wgpu::VertexBufferLayout<'_>],
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs"),
            buffers,
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

/// The vertex-buffer layout for the static unit-quad corners (slot 0, per-vertex).
fn corner_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 1] = wgpu::vertex_attr_array![0 => Float32x2];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &ATTRS,
    }
}

/// The vertex-buffer layout for the per-background-quad instance data (slot 1, per-instance).
fn quad_instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![1 => Float32x2, 2 => Float32x2, 3 => Float32x4];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<QuadInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

/// The vertex-buffer layout for the per-glyph instance data (slot 1, per-instance).
fn glyph_instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        1 => Float32x2, 2 => Float32x2, 3 => Float32x2, 4 => Float32x2, 5 => Float32x4
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<GlyphInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

/// Prefers a non-sRGB surface format so Nova's already-sRGB cell colours display 1:1 (no double
/// gamma encoding — see `docs/nova.md` §6), falling back to the surface's first supported format.
fn pick_format(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    caps.formats
        .iter()
        .copied()
        .find(|format| !format.is_srgb())
        .unwrap_or(caps.formats[0])
}

impl Gpu {
    /// Builds a [`Gpu`] for `surface` at `width` × `height`, returning it with a ready
    /// [`wgpu::SurfaceConfiguration`] (a non-sRGB format preferred — see [`pick_format`]), or `None`
    /// if no adapter supports the surface. The caller configures the surface and updates the config's
    /// `width`/`height` on resize.
    #[must_use]
    pub fn for_surface(
        instance: &wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        width: u32,
        height: u32,
    ) -> Option<(Self, wgpu::SurfaceConfiguration)> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(surface),
            ..Default::default()
        }))
        .ok()?;
        let mut config = surface.get_default_config(&adapter, width, height)?;
        config.format = pick_format(&surface.get_capabilities(&adapter));
        let gpu = Self::build(&adapter, config.format);
        Some((gpu, config))
    }

    /// Builds an off-screen [`Gpu`] (no surface), or `None` if no adapter is available — used by the
    /// renderer smoke test and, later, headless parity rendering (M4.5).
    #[must_use]
    pub fn headless() -> Option<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok()?;
        Some(Self::build(&adapter, wgpu::TextureFormat::Rgba8Unorm))
    }

    /// A throwaway off-screen colour target of `width` × `height` (for headless rendering/tests).
    #[must_use]
    pub fn offscreen_target(&self, width: u32, height: u32) -> wgpu::TextureView {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nova-offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        texture.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// The wgpu device (for configuring a surface against it).
    #[must_use]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The pixel-exact cell box of the glyph font — the front end divides the surface by this to get
    /// the cell grid, and feeds it to [`build_scene`](crate::build_scene).
    #[must_use]
    pub fn cell_metrics(&self) -> crate::CellMetrics {
        self.atlas.cell_metrics()
    }
}

#[cfg(test)]
mod tests {
    use super::Gpu;
    use crate::{build_scene, GlyphRaster};
    use penumbra::{Buffer, Rgb, Style};

    #[test]
    fn the_pipelines_build_and_render_a_frame_with_a_glyph_without_error() {
        // Verifies the GPU code is *valid* — both WGSL shaders compile, the vertex layouts match, the
        // pipelines build, a glyph rasterises into the atlas, and a frame (backgrounds + the glyph)
        // encodes/submits — all without a window. Skips where no adapter is available. Pixel-level
        // parity is M4.5.
        let Some(mut gpu) = Gpu::headless() else {
            return;
        };
        let fg = Rgb { r: 0xD9, g: 0x8E, b: 0x32 };
        let bg = Rgb { r: 0, g: 0, b: 0x27 };
        let mut buffer = Buffer::new(3, 1, Style::new(fg, bg));
        buffer.set_char(0, 0, 'X', Style::new(fg, bg)); // a glyph → exercises the atlas + glyph pass
        let metrics = GlyphRaster::new(crate::FONT_SIZE).cell_metrics();
        let scene = build_scene(&buffer, metrics);
        assert_eq!(scene.quads.len(), 3, "three cells → three background quads");
        assert_eq!(scene.glyphs.len(), 1, "one non-blank cell → one glyph");

        let view = gpu.offscreen_target(64, 32);
        gpu.render(&view, (64, 32), &scene, [0.0, 0.0, 0.153, 1.0]);
    }
}
