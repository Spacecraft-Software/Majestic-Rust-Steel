// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The winit window front end for Nova (M4.2): open a GPU window and paint a [`Scene`].
//!
//! [`run`] drives a winit event loop with an [`ApplicationHandler`]; on `resumed` it creates the
//! window + wgpu surface + [`Gpu`], and on `RedrawRequested` it renders the scene to the swapchain.
//! Input mapping (winit keys → `keymaker::KeyPress`) and a live editor frame arrive with the `App`
//! lib-split in M4.4; this chunk renders one static [`Scene`] to prove the window + pipeline.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::{Gpu, Scene};

/// Opens a GPU window and renders `scene` (cleared to `clear`, sRGB-normalised `[r, g, b, a]`),
/// blocking until the window is closed.
///
/// # Errors
/// Returns an error if the event loop cannot be created or run (no display, OS refusal, …).
pub fn run(scene: Scene, clear: [f64; 4]) -> Result<(), winit::error::EventLoopError> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = NovaApp {
        scene,
        clear,
        state: None,
    };
    event_loop.run_app(&mut app)
}

/// The live window + GPU state, created once the event loop is `resumed`.
#[derive(Debug)]
struct WindowState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    gpu: Gpu,
    config: wgpu::SurfaceConfiguration,
}

/// The winit application: the scene to paint plus the window state (absent until `resumed`).
#[derive(Debug)]
struct NovaApp {
    scene: Scene,
    clear: [f64; 4],
    state: Option<WindowState>,
}

impl ApplicationHandler for NovaApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return; // already initialised (e.g. a second `resumed` after suspend)
        }
        let attributes = Window::default_attributes()
            .with_title("Majestic — Nova (M4.2)")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 576.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("nova: could not create a window: {error}");
                event_loop.exit();
                return;
            }
        };

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = match instance.create_surface(Arc::clone(&window)) {
            Ok(surface) => surface,
            Err(error) => {
                eprintln!("nova: could not create a GPU surface: {error}");
                event_loop.exit();
                return;
            }
        };

        let size = window.inner_size();
        let Some((gpu, config)) =
            Gpu::for_surface(&instance, &surface, size.width.max(1), size.height.max(1))
        else {
            eprintln!("nova: no compatible GPU adapter found");
            event_loop.exit();
            return;
        };
        surface.configure(gpu.device(), &config);
        self.state = Some(WindowState {
            window,
            surface,
            gpu,
            config,
        });
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                state.config.width = size.width.max(1);
                state.config.height = size.height.max(1);
                state.surface.configure(state.gpu.device(), &state.config);
                state.window.request_redraw();
            }
            WindowEvent::RedrawRequested => match state.surface.get_current_texture() {
                Ok(frame) => {
                    let view = frame
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    state.gpu.render(
                        &view,
                        (state.config.width, state.config.height),
                        &self.scene,
                        self.clear,
                    );
                    frame.present();
                }
                Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                    state.surface.configure(state.gpu.device(), &state.config);
                }
                Err(error) => eprintln!("nova: dropped a frame: {error}"),
            },
            _ => {}
        }
    }
}
