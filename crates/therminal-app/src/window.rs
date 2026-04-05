//! Winit 0.30 window with wgpu surface for Therminal.
//!
//! Implements the full terminal pipeline:
//!   Keyboard (winit) -> encode_key() -> PTY write
//!   PTY read -> vte::ansi::Processor -> Term -> damage
//!   Damage -> grid_renderer.render() -> wgpu surface -> winit window
//!   Resize -> recalculate cols/rows -> resize PTY + Term

use std::collections::HashSet;
use std::io::{Read as IoRead, Write as IoWrite};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi;
use anyhow::Result;
use portable_pty::MasterPty;
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::grid_renderer::{cell_display_text, FontConfig, GridRenderer, RenderCell};
use therminal_terminal::input::{
    self, KeyCode, Modifiers as InputModifiers, MouseButton as InputMouseButton,
};

// ── Custom event for waking the event loop from the PTY reader ───────────

/// Events sent from the PTY reader thread to the winit event loop.
#[derive(Debug)]
enum UserEvent {
    /// New bytes are available from the PTY; request a redraw.
    PtyOutput,
}

// ── EventListener for alacritty_terminal::Term ──────────────────────────

/// Listener that forwards terminal events (title changes, bell, etc.)
/// Currently a minimal implementation -- events are logged but not acted on.
struct TherminalListener;

impl EventListener for TherminalListener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => debug!("Terminal title: {title}"),
            TermEvent::Wakeup => { /* handled by PTY reader thread */ }
            _ => debug!("Terminal event: {event:?}"),
        }
    }
}

// ── Dimensions adapter for alacritty_terminal ────────────────────────────

/// Simple (cols, rows) pair implementing `alacritty_terminal::grid::Dimensions`.
struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

// ── Background color (matches grid_renderer TERM_BG) ─────────────────────

const BG_COLOR: wgpu::Color = wgpu::Color {
    r: 10.0 / 255.0,
    g: 0.0,
    b: 16.0 / 255.0,
    a: 1.0,
};

// ── GPU state ────────────────────────────────────────────────────────────

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

// ── Main application ─────────────────────────────────────────────────────

/// Main application struct implementing winit's `ApplicationHandler`.
pub struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    grid_renderer: Option<GridRenderer>,

    /// Shared terminal state (alacritty_terminal::Term behind FairMutex).
    term: Option<Arc<FairMutex<Term<TherminalListener>>>>,

    /// VTE parser for feeding PTY bytes to the Term. Owned by the PTY
    /// reader thread, so we don't store it here.

    /// PTY master write handle -- keyboard input is written here.
    pty_writer: Option<Box<dyn IoWrite + Send>>,

    /// PTY master (kept alive so the PTY doesn't close).
    _pty_master: Option<Box<dyn MasterPty + Send>>,

    /// Proxy to wake the event loop from the PTY reader thread.
    event_proxy: EventLoopProxy<UserEvent>,

    /// Current modifiers state from winit.
    modifiers: Modifiers,

    /// Trailing-edge resize debounce: stores the most recent size that
    /// arrived too soon after the last applied resize, plus the timestamp
    /// of the last applied resize. Flushed on the next RedrawRequested.
    pending_resize: Option<PhysicalSize<u32>>,
    last_resize_at: Option<Instant>,

    /// Current cursor position in physical pixels (updated on CursorMoved).
    cursor_position: Option<(f64, f64)>,

    /// Whether the left mouse button is currently held (for drag reporting).
    mouse_left_held: bool,
}

impl App {
    fn new(event_proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            window: None,
            gpu: None,
            grid_renderer: None,
            term: None,
            pty_writer: None,
            _pty_master: None,
            event_proxy,
            modifiers: Modifiers::default(),
            pending_resize: None,
            last_resize_at: None,
            cursor_position: None,
            mouse_left_held: false,
        }
    }

    /// Initialize wgpu, grid renderer, terminal, and PTY.
    fn init_gpu(&mut self, window: Arc<Window>) {
        let size = window.inner_size();
        let backends = if cfg!(target_os = "linux") {
            wgpu::Backends::VULKAN
        } else {
            wgpu::Backends::all()
        };
        info!("wgpu backends: {:?}", backends);
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });

        let surface = instance
            .create_surface(window.clone())
            .expect("failed to create wgpu surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("no suitable GPU adapter found");

        info!(
            "wgpu adapter: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("therminal"),
                ..Default::default()
            },
            None,
        ))
        .expect("failed to create wgpu device");

        let surface_caps = surface.get_capabilities(&adapter);
        let format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or_else(|| {
                surface_caps
                    .formats
                    .first()
                    .copied()
                    .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb)
            });

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: *surface_caps
                .alpha_modes
                .iter()
                .find(|m| **m == wgpu::CompositeAlphaMode::Opaque)
                .or_else(|| {
                    surface_caps
                        .alpha_modes
                        .iter()
                        .find(|m| **m == wgpu::CompositeAlphaMode::Auto)
                })
                .unwrap_or_else(|| {
                    surface_caps
                        .alpha_modes
                        .first()
                        .unwrap_or(&wgpu::CompositeAlphaMode::Opaque)
                }),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        info!("wgpu alpha_mode: {:?}", config.alpha_mode);
        surface.configure(&device, &config);

        // ── Grid renderer ────────────────────────────────────────────────
        let scale = window.scale_factor() as f32;
        let mut font_config = FontConfig::default();
        // Scale font size to physical pixels so glyphs fill the correct cell area.
        font_config.font_size *= scale;
        font_config.line_height = font_config.font_size * 1.4;
        info!(
            scale,
            font_size = font_config.font_size,
            "Applying DPI scale to font"
        );
        let grid_renderer = GridRenderer::new(
            &device,
            &queue,
            format,
            config.width,
            config.height,
            font_config,
        );

        // ── Terminal (alacritty_terminal::Term) ──────────────────────────
        let (cols, rows) = grid_renderer.grid_size(config.width, config.height);
        let term_config = TermConfig {
            scrolling_history: 10_000,
            ..Default::default()
        };
        let term_size = TermSize {
            columns: cols,
            screen_lines: rows,
        };
        let term = Term::new(term_config, &term_size, TherminalListener);
        let term = Arc::new(FairMutex::new(term));

        info!("Terminal created: {cols}x{rows}");

        // ── PTY ──────────────────────────────────────────────────────────
        let (pty_master, _child) = therminal_terminal::pty::spawn_shell(cols as u16, rows as u16)
            .expect("failed to spawn shell");

        let pty_reader = pty_master
            .try_clone_reader()
            .expect("failed to clone PTY reader");
        let pty_writer = pty_master.take_writer().expect("failed to get PTY writer");

        // ── Spawn PTY reader thread ──────────────────────────────────────
        let term_for_reader = Arc::clone(&term);
        let proxy = self.event_proxy.clone();
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                pty_reader_loop(pty_reader, term_for_reader, proxy);
            })
            .expect("failed to spawn PTY reader thread");

        self.window = Some(window);
        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
        });
        self.grid_renderer = Some(grid_renderer);
        self.term = Some(term);
        self.pty_writer = Some(pty_writer);
        self._pty_master = Some(pty_master);
    }

    /// Resize the surface, terminal, and PTY to match the new window size.
    fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }

        let gpu = match self.gpu.as_mut() {
            Some(g) => g,
            None => return,
        };

        gpu.config.width = new_size.width;
        gpu.config.height = new_size.height;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gpu.surface.configure(&gpu.device, &gpu.config);
        }));
        if let Err(e) = result {
            warn!("Surface configure panicked during resize: {:?}", e);
            return;
        }

        // Resize grid renderer.
        if let Some(renderer) = self.grid_renderer.as_mut() {
            renderer.resize(&gpu.device, &gpu.queue, new_size.width, new_size.height);

            // Recalculate terminal grid dimensions.
            let (cols, rows) = renderer.grid_size(new_size.width, new_size.height);

            // Resize alacritty_terminal Term.
            if let Some(term) = self.term.as_ref() {
                let mut term_guard = term.lock();
                let term_size = TermSize {
                    columns: cols,
                    screen_lines: rows,
                };
                term_guard.resize(term_size);
            }

            // Resize PTY.
            if let Some(master) = self._pty_master.as_ref() {
                if let Err(e) =
                    therminal_terminal::pty::resize(master.as_ref(), cols as u16, rows as u16)
                {
                    warn!("Failed to resize PTY: {e}");
                }
            }

            debug!(
                "Resized to {}x{} ({cols}x{rows} cells)",
                new_size.width, new_size.height
            );
        }
    }

    /// Render a frame: read terminal content and submit to GPU.
    fn render(&mut self) {
        let gpu = match self.gpu.as_ref() {
            Some(g) => g,
            None => return,
        };
        let renderer = match self.grid_renderer.as_mut() {
            Some(r) => r,
            None => return,
        };
        let term_arc = match self.term.as_ref() {
            Some(t) => t,
            None => return,
        };

        let output = match gpu.surface.get_current_texture() {
            Ok(tex) => tex,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                warn!("wgpu: out of memory");
                return;
            }
            Err(e) => {
                warn!("wgpu surface error: {e}");
                return;
            }
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("therminal_render"),
            });

        // Clear to background color.
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(BG_COLOR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        // Lock the terminal, extract cells and damage info, then release lock.
        let mut term_guard = term_arc.lock();

        // Get damage info.
        let damaged_rows = match term_guard.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(iter) => {
                let set: HashSet<usize> = iter
                    .filter_map(|bounds| {
                        if bounds.is_damaged() {
                            Some(bounds.line)
                        } else {
                            None
                        }
                    })
                    .collect();
                if set.is_empty() {
                    // Nothing damaged -- render from cache.
                    Some(set)
                } else {
                    Some(set)
                }
            }
        };

        let content = term_guard.renderable_content();
        let screen_lines = term_guard.screen_lines();
        let display_offset = content.display_offset;
        let cursor = content.cursor;
        let selection_range = content.selection;

        // Collect cells into RenderCell snapshots.
        let cells: Vec<RenderCell> = content
            .display_iter
            .filter_map(|indexed| {
                let point = indexed.point;
                let cell = indexed.cell;

                // Convert grid line to viewport row index.
                let viewport_line = point.line.0 + display_offset as i32;
                let row = usize::try_from(viewport_line).ok()?;
                if row >= screen_lines {
                    return None;
                }

                // Skip wide char spacers.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    return None;
                }

                let hyperlink = cell.hyperlink().map(|h| h.uri().to_owned());

                Some(RenderCell {
                    row,
                    col: point.column.0,
                    c: cell.c,
                    text: cell_display_text(cell.c, cell.zerowidth()),
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    hyperlink,
                })
            })
            .collect();

        // Reset damage while we still hold the lock.
        term_guard.reset_damage();
        drop(term_guard);

        // Render the grid.
        renderer.render(
            &cells,
            &cursor,
            screen_lines,
            selection_range.as_ref(),
            display_offset,
            damaged_rows.as_ref(),
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            &view,
            gpu.config.width,
            gpu.config.height,
        );

        gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }

    /// Handle a keyboard event: encode it and write to the PTY.
    fn handle_key_input(&mut self, key_event: &KeyEvent) {
        let pty = match self.pty_writer.as_mut() {
            Some(w) => w,
            None => return,
        };

        // Map winit key to our platform-agnostic KeyCode.
        let key_code = match &key_event.logical_key {
            Key::Named(named) => match named {
                NamedKey::Enter => Some(KeyCode::Enter),
                NamedKey::Backspace => Some(KeyCode::Backspace),
                NamedKey::Tab => Some(KeyCode::Tab),
                NamedKey::Escape => Some(KeyCode::Escape),
                NamedKey::ArrowUp => Some(KeyCode::ArrowUp),
                NamedKey::ArrowDown => Some(KeyCode::ArrowDown),
                NamedKey::ArrowLeft => Some(KeyCode::ArrowLeft),
                NamedKey::ArrowRight => Some(KeyCode::ArrowRight),
                NamedKey::Home => Some(KeyCode::Home),
                NamedKey::End => Some(KeyCode::End),
                NamedKey::PageUp => Some(KeyCode::PageUp),
                NamedKey::PageDown => Some(KeyCode::PageDown),
                NamedKey::Insert => Some(KeyCode::Insert),
                NamedKey::Delete => Some(KeyCode::Delete),
                NamedKey::F1 => Some(KeyCode::F1),
                NamedKey::F2 => Some(KeyCode::F2),
                NamedKey::F3 => Some(KeyCode::F3),
                NamedKey::F4 => Some(KeyCode::F4),
                NamedKey::F5 => Some(KeyCode::F5),
                NamedKey::F6 => Some(KeyCode::F6),
                NamedKey::F7 => Some(KeyCode::F7),
                NamedKey::F8 => Some(KeyCode::F8),
                NamedKey::F9 => Some(KeyCode::F9),
                NamedKey::F10 => Some(KeyCode::F10),
                NamedKey::F11 => Some(KeyCode::F11),
                NamedKey::F12 => Some(KeyCode::F12),
                _ => None,
            },
            Key::Character(s) => {
                // For character keys, take the first char.
                s.chars().next().map(KeyCode::Char)
            }
            _ => None,
        };

        let key_code = match key_code {
            Some(k) => k,
            None => return,
        };

        // Map winit modifier state.
        let state = self.modifiers.state();
        let mods = InputModifiers {
            ctrl: state.control_key(),
            alt: state.alt_key(),
            shift: state.shift_key(),
        };

        // Encode and write to PTY.
        if let Some(bytes) = input::encode_key(&key_code, &mods) {
            if let Err(e) = pty.write_all(&bytes) {
                warn!("Failed to write to PTY: {e}");
            }
        }
    }

    /// Convert physical pixel coordinates to terminal grid (col, row).
    ///
    /// Returns `None` if the coordinates fall outside the grid area or
    /// if the grid renderer is not initialized.
    fn pixel_to_grid(&self, px: f64, py: f64) -> Option<(usize, usize)> {
        let renderer = self.grid_renderer.as_ref()?;
        let col = ((px as f32 - renderer.padding_x) / renderer.cell_width).floor();
        let row = ((py as f32 - renderer.padding_y) / renderer.cell_height).floor();
        if col < 0.0 || row < 0.0 {
            return None;
        }
        let col = col as usize;
        let row = row as usize;

        // Clamp to terminal grid bounds.
        let term = self.term.as_ref()?;
        let term_guard = term.lock();
        let max_col = term_guard.columns().saturating_sub(1);
        let max_row = term_guard.screen_lines().saturating_sub(1);
        Some((col.min(max_col), row.min(max_row)))
    }

    /// Get the current terminal mode flags, or empty if term is not initialized.
    fn term_mode(&self) -> TermMode {
        self.term
            .as_ref()
            .map(|t| *t.lock().mode())
            .unwrap_or_default()
    }

    /// Get input modifiers from the current winit modifier state.
    fn input_mods(&self) -> InputModifiers {
        let state = self.modifiers.state();
        InputModifiers {
            ctrl: state.control_key(),
            alt: state.alt_key(),
            shift: state.shift_key(),
        }
    }

    /// Write bytes to the PTY.
    fn pty_write(&mut self, bytes: &[u8]) {
        if let Some(pty) = self.pty_writer.as_mut() {
            if let Err(e) = pty.write_all(bytes) {
                warn!("Failed to write to PTY: {e}");
            }
        }
    }

    /// Handle mouse scroll: either scroll the terminal scrollback or forward
    /// as SGR mouse events to the running program.
    fn handle_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        let (col, row) = self
            .cursor_position
            .and_then(|(px, py)| self.pixel_to_grid(px, py))
            .unwrap_or((0, 0));

        let lines = match delta {
            MouseScrollDelta::LineDelta(_x, y) => y,
            MouseScrollDelta::PixelDelta(pos) => {
                // Convert pixel delta to approximate line count.
                let cell_h = self
                    .grid_renderer
                    .as_ref()
                    .map(|r| r.cell_height)
                    .unwrap_or(20.0);
                (pos.y as f32) / cell_h
            }
        };

        if lines.abs() < 0.01 {
            return;
        }

        let mode = self.term_mode();
        let mouse_mode = mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_DRAG)
            || mode.contains(TermMode::MOUSE_MOTION);

        if mouse_mode {
            // Forward scroll as SGR mouse events to the running program.
            let mods = self.input_mods();
            let button = if lines > 0.0 {
                InputMouseButton::ScrollUp
            } else {
                InputMouseButton::ScrollDown
            };
            let steps = lines.abs().ceil() as usize;
            let mut seq = Vec::new();
            for _ in 0..steps {
                seq.extend_from_slice(&input::encode_mouse_press(button, col, row, &mods));
            }
            self.pty_write(&seq);
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // In alt screen with alternate scroll: send arrow key presses.
            let steps = lines.abs().ceil() as usize;
            let arrow = if lines > 0.0 { b"\x1b[A" } else { b"\x1b[B" };
            let mut seq = Vec::with_capacity(steps * 3);
            for _ in 0..steps {
                seq.extend_from_slice(arrow);
            }
            self.pty_write(&seq);
        } else {
            // Normal scrollback navigation.
            if let Some(term) = self.term.as_ref() {
                let scroll_lines = (lines * 3.0).round() as i32;
                let mut term_guard = term.lock();
                term_guard.scroll_display(Scroll::Delta(scroll_lines));
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Handle mouse button press/release.
    fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let (col, row) = match self
            .cursor_position
            .and_then(|(px, py)| self.pixel_to_grid(px, py))
        {
            Some(pos) => pos,
            None => return,
        };

        // Track left button state for drag reporting.
        if button == MouseButton::Left {
            self.mouse_left_held = state == ElementState::Pressed;
        }

        let mode = self.term_mode();
        let mouse_mode = mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_DRAG)
            || mode.contains(TermMode::MOUSE_MOTION);

        if !mouse_mode {
            return;
        }

        let input_button = match button {
            MouseButton::Left => InputMouseButton::Left,
            MouseButton::Middle => InputMouseButton::Middle,
            MouseButton::Right => InputMouseButton::Right,
            _ => return,
        };

        let mods = self.input_mods();
        let bytes = match state {
            ElementState::Pressed => input::encode_mouse_press(input_button, col, row, &mods),
            ElementState::Released => input::encode_mouse_release(input_button, col, row, &mods),
        };
        self.pty_write(&bytes);
    }

    /// Handle mouse motion for drag/motion reporting.
    fn handle_cursor_moved(&mut self, position: winit::dpi::PhysicalPosition<f64>) {
        let (px, py) = (position.x, position.y);
        self.cursor_position = Some((px, py));

        let (col, row) = match self.pixel_to_grid(px, py) {
            Some(pos) => pos,
            None => return,
        };

        let mode = self.term_mode();

        if self.mouse_left_held
            && (mode.contains(TermMode::MOUSE_DRAG) || mode.contains(TermMode::MOUSE_MOTION))
        {
            let mods = self.input_mods();
            let bytes = input::encode_mouse_drag(InputMouseButton::Left, col, row, &mods);
            self.pty_write(&bytes);
        } else if mode.contains(TermMode::MOUSE_MOTION) {
            let mods = self.input_mods();
            let bytes = input::encode_mouse_motion(col, row, &mods);
            self.pty_write(&bytes);
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("Therminal")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );

        let scale = window.scale_factor();
        info!("window created (scale_factor={scale:.2})");

        self.init_gpu(window);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyOutput => {
                // New PTY data available -- request a redraw.
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested, exiting");
                event_loop.exit();
            }

            WindowEvent::Resized(new_size) => {
                // Ignore zero-size resizes (e.g. minimize) — don't let them
                // overwrite a valid pending size.
                if new_size.width == 0 || new_size.height == 0 {
                    return;
                }
                let now = Instant::now();
                let elapsed_ok = self
                    .last_resize_at
                    .map(|t| now.duration_since(t).as_millis() > 16)
                    .unwrap_or(true);
                if elapsed_ok {
                    self.last_resize_at = Some(now);
                    self.pending_resize = None;
                    self.resize(new_size);
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                } else {
                    // Stash for trailing-edge flush on next RedrawRequested.
                    self.pending_resize = Some(new_size);
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                info!("scale factor changed to {scale_factor:.2}");
                let new_size = self.window.as_ref().map(|w| w.inner_size());
                if let Some(size) = new_size {
                    self.resize(size);
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                // Flush any pending trailing-edge resize before rendering.
                if let Some(size) = self.pending_resize.take() {
                    self.last_resize_at = Some(Instant::now());
                    self.resize(size);
                }
                self.render();
            }

            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.handle_cursor_moved(position);
            }

            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_mouse_wheel(delta);
            }

            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input(state, button);
            }

            WindowEvent::KeyboardInput {
                event:
                    ref key_event @ KeyEvent {
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                self.handle_key_input(key_event);
            }

            _ => {}
        }
    }
}

// ── PTY reader thread ────────────────────────────────────────────────────

/// Reads PTY output in a loop, feeds bytes to the Term via the VTE parser,
/// and wakes the event loop for redraw.
///
/// An optional [`TherminalInterceptor`] gets first crack at OSC/DCS/APC
/// sequences before they reach the terminal handler.  This is how therminal
/// detects AI-agent shell-integration escapes without forking alacritty_terminal.
fn pty_reader_loop(
    mut reader: Box<dyn IoRead + Send>,
    term: Arc<FairMutex<Term<TherminalListener>>>,
    proxy: EventLoopProxy<UserEvent>,
) {
    use therminal_terminal::interceptor::{InterceptorConfig, TherminalInterceptor};

    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();
    // Keep the receiver alive so `interceptor.send()` calls don't fail.
    // Events will be consumed once we wire up semantic event handling.
    let (mut interceptor, _event_rx) = TherminalInterceptor::new(InterceptorConfig::default());
    let mut buf = [0u8; 4096];

    info!("PTY reader thread started (with SequenceInterceptor)");

    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                info!("PTY closed (EOF)");
                break;
            }
            Ok(n) => {
                // Feed bytes to the terminal under the lock.
                // The interceptor sees OSC/DCS/APC sequences first.
                {
                    let mut term_guard = term.lock();
                    processor.advance_with_interceptor(
                        &mut *term_guard,
                        &mut interceptor,
                        &buf[..n],
                    );
                }

                // Wake the event loop to trigger a redraw.
                let _ = proxy.send_event(UserEvent::PtyOutput);
            }
            Err(e) => {
                warn!("PTY read error: {e}");
                break;
            }
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────

/// Create the event loop, set control flow to Wait, and run the app.
pub fn run() -> Result<()> {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Therminal panic: {info}");
        eprintln!("Backtrace: {:?}", std::backtrace::Backtrace::capture());
    }));

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app)?;

    Ok(())
}
