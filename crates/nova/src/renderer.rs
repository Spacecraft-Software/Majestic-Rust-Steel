// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The wgpu side of Nova (M4.2): turn a [`Scene`](crate::Scene)'s background quads into pixels.
//!
//! [`Gpu`] owns the device, queue, and a single instanced-rectangle pipeline. [`Gpu::render`]
//! clears the target to the Steelbore background and draws one instanced quad per cell background.
//! Glyphs (the `cosmic-text` pass) land in M4.3; this chunk proves the window + GPU pipeline by
//! painting the editor's cell *layout* as coloured rectangles.
//!
//! No `unsafe` (§6.1): the surface is created from a borrowed window through wgpu's safe
//! `create_surface`, and `bytemuck` keeps the vertex-byte casts inside its own crate.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt as _;

use crate::Scene;

/// The instanced-rectangle shader. Each instance is a pixel-space rect + colour; the vertex stage
/// maps it to clip space using the surface size, the fragment stage emits the colour flat.
const SHADER: &str = r"
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

/// A wgpu device + queue + the single instanced-quad pipeline. Render a [`Scene`] with
/// [`Gpu::render`]; create one for a window with [`Gpu::for_surface`] or off-screen with
/// [`Gpu::headless`].
#[derive(Debug)]
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    corners: wgpu::Buffer,
    screen: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl Gpu {
    /// Builds the device, queue, and instanced-quad pipeline targeting `format` from `adapter`.
    fn build(adapter: &wgpu::Adapter, format: wgpu::TextureFormat) -> Self {
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("nova-device"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .expect("the adapter must provide a device");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nova-quads"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nova-screen"),
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
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nova-layout"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nova-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[corner_layout(), instance_layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
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
        });

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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nova-bind"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen.as_entire_binding(),
            }],
        });

        Self {
            device,
            queue,
            pipeline,
            corners,
            screen,
            bind_group,
        }
    }

    /// Clears `view` to `clear` (sRGB-normalised `[r, g, b, a]`) and draws `scene`'s background quads,
    /// sized to a `size`-pixel surface. The caller presents/holds the target.
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

        let instances: Vec<QuadInstance> = scene
            .quads
            .iter()
            .map(|quad| QuadInstance {
                pos: [quad.x, quad.y],
                size: [quad.width, quad.height],
                color: quad.color,
            })
            .collect();
        let instance_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("nova-instances"),
            contents: bytemuck::cast_slice(&instances),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let count = u32::try_from(instances.len()).unwrap_or(u32::MAX);

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
            if count > 0 {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.corners.slice(..));
                pass.set_vertex_buffer(1, instance_buf.slice(..));
                pass.draw(0..6, 0..count);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
    }
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

/// The vertex-buffer layout for the per-quad instance data (slot 1, per-instance).
fn instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![1 => Float32x2, 2 => Float32x2, 3 => Float32x4];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<QuadInstance>() as wgpu::BufferAddress,
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
}

#[cfg(test)]
mod tests {
    use super::Gpu;
    use crate::{build_scene, CellMetrics};
    use penumbra::{Buffer, Rgb, Style};

    #[test]
    fn the_pipeline_builds_and_renders_a_frame_without_error() {
        // Verifies the GPU code is *valid* — the WGSL compiles, the vertex layouts match it, the
        // pipeline builds, and a frame encodes/submits — without a window. Skips where no adapter is
        // available (e.g. a headless CI box with no software rasteriser). Pixel-level parity is M4.5.
        let Some(mut gpu) = Gpu::headless() else {
            return;
        };
        let fg = Rgb { r: 0xD9, g: 0x8E, b: 0x32 };
        let bg = Rgb { r: 0, g: 0, b: 0x27 };
        let mut buffer = Buffer::new(2, 1, Style::new(fg, bg));
        buffer.set_char(0, 0, 'X', Style::new(fg, bg));
        let scene = build_scene(&buffer, CellMetrics::new(8.0, 16.0));
        assert_eq!(scene.quads.len(), 2, "two cells → two background quads");

        let view = gpu.offscreen_target(16, 16);
        gpu.render(&view, (16, 16), &scene, [0.0, 0.0, 0.153, 1.0]);
    }
}
