// SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
// SPDX-License-Identifier: GPL-3.0-or-later

//! The winit window front end for Nova (M4.4): run the live Majestic editor in a GPU window.
//!
//! [`run_editor`] drives a winit event loop with an [`ApplicationHandler`] that owns a [`majestic::App`]
//! — the *same* editor the TTY runs. Each frame it sizes a cell [`Buffer`] to the window (so the editor
//! reflows on resize), has the `App` render into it, turns that into a [`Scene`](crate::Scene), and
//! paints it with the Nova renderer. winit key events are mapped to `keymaker::KeyPress` and fed to the
//! `App`. This is the GUI half of the renderer-parity contract: one `App`, two front ends (PRD §6.5).

use std::sync::Arc;
use std::time::{Duration, Instant};

use keymaker::{KeyCode, KeyPress, Mods};
use majestic::App;
use penumbra::{Buffer, Rgb, Theme};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::{build_scene, Gpu};

/// Runs `app` (the live editor) in a GPU window, blocking until the window is closed.
///
/// # Errors
/// Returns an error if the event loop cannot be created or run (no display, OS refusal, …).
pub fn run_editor(app: App) -> Result<(), winit::error::EventLoopError> {
    let event_loop = EventLoop::new()?;
    let theme = Theme::steelbore();
    let mut nova = NovaApp {
        app,
        clear: clear_color(&theme),
        theme,
        buffer: Buffer::new(1, 1, Theme::steelbore().base_style()),
        modifiers: ModifiersState::empty(),
        state: None,
        clipboard: arboard::Clipboard::new().ok(),
        last_clipboard: String::new(),
    };
    event_loop.run_app(&mut nova)
}

/// The live window + GPU state, created once the event loop is `resumed`.
#[derive(Debug)]
struct WindowState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    gpu: Gpu,
    config: wgpu::SurfaceConfiguration,
}

/// The winit application: the live editor, the theme + clear colour, the cell buffer it renders into,
/// the tracked modifier state, the window/GPU state (absent until `resumed`), and the system-clipboard
/// bridge (absent if the compositor's clipboard is unreachable — the editor's kill-ring still works).
struct NovaApp {
    app: App,
    theme: Theme,
    clear: [f64; 4],
    buffer: Buffer,
    modifiers: ModifiersState,
    state: Option<WindowState>,
    clipboard: Option<arboard::Clipboard>,
    last_clipboard: String,
}

impl ApplicationHandler for NovaApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("Majestic — Nova")
            .with_inner_size(winit::dpi::LogicalSize::new(1100.0, 720.0));
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
        self.sync_clipboard_in(); // seed the kill-ring from the system clipboard at startup
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            // On (re)focus, pull the system clipboard in, so a paste inserts whatever was copied
            // elsewhere — the common "copy in another app, paste here" flow.
            WindowEvent::Focused(true) => self.sync_clipboard_in(),
            WindowEvent::Resized(size) => {
                if let Some(state) = self.state.as_mut() {
                    state.config.width = size.width.max(1);
                    state.config.height = size.height.max(1);
                    state.surface.configure(state.gpu.device(), &state.config);
                    state.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let Some(press) = translate(&event.logical_key, self.modifiers) {
                    self.feed_key(event_loop, press);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Re-render at ~60 Hz so the live terminal/agent output and the cursor animate, without
        // spinning a core flat-out.
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(16),
        ));
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }
}

impl NovaApp {
    /// Feeds `press` to the editor at the current cell grid, exiting if the editor asks to quit.
    fn feed_key(&mut self, event_loop: &ActiveEventLoop, press: KeyPress) {
        let Some((cols, rows)) = self.grid() else {
            return;
        };
        if let Err(error) = self.app.handle_key(press, cols, rows) {
            eprintln!("nova: key handling error: {error}");
        }
        self.sync_clipboard_out(); // a copy/cut chord just ran? push it to the system clipboard
        if self.app.should_quit() {
            event_loop.exit();
        }
    }

    /// Pulls the system clipboard into the editor's shared clipboard, so the next paste inserts it.
    /// Best effort: a clipboard that is absent or unreadable simply leaves the kill-ring as it was.
    fn sync_clipboard_in(&mut self) {
        if let Some(clipboard) = self.clipboard.as_mut() {
            if let Ok(text) = clipboard.get_text() {
                self.app.set_clipboard(&text);
                self.last_clipboard = text;
            }
        }
    }

    /// Pushes the editor's shared clipboard out to the system clipboard when it has changed (after a
    /// copy or cut), so other applications can paste it. Best effort; writes only on a real change.
    fn sync_clipboard_out(&mut self) {
        if self.app.clipboard() == self.last_clipboard {
            return;
        }
        self.last_clipboard.clear();
        self.last_clipboard.push_str(self.app.clipboard());
        if let Some(clipboard) = self.clipboard.as_mut() {
            let _ = clipboard.set_text(self.last_clipboard.clone());
        }
    }

    /// Renders one frame: size the cell buffer to the window, let the editor draw into it, and paint it.
    fn redraw(&mut self) {
        let Some((cols, rows)) = self.grid() else {
            return;
        };
        if self.buffer.width() != cols || self.buffer.height() != rows {
            self.buffer.resize(cols, rows, self.theme.base_style());
        }
        self.app.tick();
        self.app.render(&mut self.buffer, &self.theme);
        let scene = {
            let state = self.state.as_ref().expect("grid() returned Some, so state exists");
            build_scene(&self.buffer, state.gpu.cell_metrics())
        };
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match state.surface.get_current_texture() {
            Ok(frame) => {
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                state.gpu.render(
                    &view,
                    (state.config.width, state.config.height),
                    &scene,
                    self.clear,
                );
                frame.present();
            }
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                state.surface.configure(state.gpu.device(), &state.config);
            }
            Err(error) => eprintln!("nova: dropped a frame: {error}"),
        }
    }

    /// The cell grid (`columns`, `rows`) for the current window size, or `None` if no window yet.
    fn grid(&self) -> Option<(u16, u16)> {
        let state = self.state.as_ref()?;
        Some(cell_grid(
            state.config.width,
            state.config.height,
            state.gpu.cell_metrics(),
        ))
    }
}

/// The window's cell grid: the surface size divided by the cell box, clamped to `1..=u16::MAX`.
fn cell_grid(width: u32, height: u32, metrics: crate::CellMetrics) -> (u16, u16) {
    let cols = (f64::from(width) / f64::from(metrics.width)).floor();
    let rows = (f64::from(height) / f64::from(metrics.height)).floor();
    (clamp_grid(cols), clamp_grid(rows))
}

/// Clamps a (finite, non-negative) cell count to `1..=u16::MAX`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to [1, u16::MAX] and floored before the cast"
)]
fn clamp_grid(value: f64) -> u16 {
    value.clamp(1.0, f64::from(u16::MAX)) as u16
}

/// The Steelbore background as a wgpu clear colour (sRGB-normalised).
fn clear_color(theme: &Theme) -> [f64; 4] {
    let Rgb { r, g, b } = theme.background;
    [
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
        1.0,
    ]
}

/// Maps a winit logical key + the current modifier state to a Keymaker [`KeyPress`], if it maps to
/// one. winit's `logical_key` already reflects the layout + Shift, so `Ctrl+A` arrives as
/// `Character("a")` with the Control modifier — exactly what the keymaps expect.
fn translate(key: &Key, mods: ModifiersState) -> Option<KeyPress> {
    let mut press = Mods::NONE;
    if mods.control_key() {
        press |= Mods::CTRL;
    }
    if mods.alt_key() {
        press |= Mods::ALT;
    }
    if mods.shift_key() {
        press |= Mods::SHIFT;
    }
    if mods.super_key() {
        press |= Mods::SUPER;
    }
    let code = match key {
        Key::Character(text) => KeyCode::Char(text.chars().next()?),
        Key::Named(named) => named_code(*named)?,
        _ => return None,
    };
    Some(KeyPress::new(press, code))
}

/// Maps a winit [`NamedKey`] to a Keymaker [`KeyCode`], or `None` for keys the editor ignores.
fn named_code(named: NamedKey) -> Option<KeyCode> {
    Some(match named {
        NamedKey::Enter => KeyCode::Enter,
        NamedKey::Backspace => KeyCode::Backspace,
        NamedKey::Escape => KeyCode::Escape,
        NamedKey::Tab => KeyCode::Tab,
        NamedKey::Space => KeyCode::Char(' '),
        NamedKey::ArrowLeft => KeyCode::Left,
        NamedKey::ArrowRight => KeyCode::Right,
        NamedKey::ArrowUp => KeyCode::Up,
        NamedKey::ArrowDown => KeyCode::Down,
        NamedKey::Home => KeyCode::Home,
        NamedKey::End => KeyCode::End,
        NamedKey::PageUp => KeyCode::PageUp,
        NamedKey::PageDown => KeyCode::PageDown,
        NamedKey::Delete => KeyCode::Delete,
        NamedKey::Insert => KeyCode::Insert,
        NamedKey::F1 => KeyCode::Function(1),
        NamedKey::F2 => KeyCode::Function(2),
        NamedKey::F3 => KeyCode::Function(3),
        NamedKey::F4 => KeyCode::Function(4),
        NamedKey::F5 => KeyCode::Function(5),
        NamedKey::F6 => KeyCode::Function(6),
        NamedKey::F7 => KeyCode::Function(7),
        NamedKey::F8 => KeyCode::Function(8),
        NamedKey::F9 => KeyCode::Function(9),
        NamedKey::F10 => KeyCode::Function(10),
        NamedKey::F11 => KeyCode::Function(11),
        NamedKey::F12 => KeyCode::Function(12),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{named_code, translate};
    use keymaker::{KeyCode, KeyPress, Mods};
    use winit::keyboard::{Key, ModifiersState, NamedKey};

    #[test]
    fn ctrl_a_translates_to_a_control_chord() {
        // winit's logical key for Ctrl+A is the base character; the modifier rides separately.
        let mut mods = ModifiersState::empty();
        mods.insert(ModifiersState::CONTROL);
        let key = Key::Character("a".into());
        assert_eq!(translate(&key, mods), Some(KeyPress::ctrl('a')));
    }

    #[test]
    fn named_keys_map_to_their_codes() {
        assert_eq!(named_code(NamedKey::Enter), Some(KeyCode::Enter));
        assert_eq!(named_code(NamedKey::ArrowLeft), Some(KeyCode::Left));
        assert_eq!(named_code(NamedKey::F12), Some(KeyCode::Function(12)));
        assert_eq!(named_code(NamedKey::Space), Some(KeyCode::Char(' ')));
        let plain = translate(&Key::Named(NamedKey::Escape), ModifiersState::empty());
        assert_eq!(plain, Some(KeyPress::new(Mods::NONE, KeyCode::Escape)));
    }
}
