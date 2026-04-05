//! Winit 0.30 window with wgpu surface for Therminal.
//!
//! Implements the full terminal pipeline with split-pane support:
//!   Keyboard (winit) -> encode_key() -> focused pane's PTY write
//!   PTY read -> vte::ansi::Processor -> Term -> damage
//!   Damage -> grid_renderer.render() per pane -> wgpu surface -> winit window
//!   Resize -> recalculate layout tree -> resize all pane PTYs + Terms
//!
//! Keyboard shortcuts (all Ctrl+Shift):
//!   H  -- split focused pane horizontally (side-by-side)
//!   V  -- split focused pane vertically (top/bottom)
//!   ArrowLeft / ArrowRight -- move focus prev / next
//!   W  -- close focused pane
//!   =  -- grow focused pane's split ratio
//!   -  -- shrink focused pane's split ratio

use std::collections::HashSet;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{TermDamage, TermMode};
use anyhow::Result;
use tracing::{debug, info, warn};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::grid_renderer::{cell_display_text, ColorVertex, FontConfig, GridRenderer, RenderCell};
use crate::pane::{FocusDirection, LayoutNode, PaneId, PaneState, SplitDirection};
use alacritty_terminal::grid::Dimensions;
use therminal_core::config::TherminalConfig;
use therminal_core::config_watcher::{ConfigChanged, ConfigWatcher};
use therminal_core::geometry::Rect;
use therminal_core::palette::Color as PaletteColor;
use therminal_terminal::input::{
    self, KeyCode, Modifiers as InputModifiers, MouseButton as InputMouseButton,
};
use therminal_terminal::interceptor::InterceptorConfig;

// ── Custom event for waking the event loop from the PTY reader ───────────

/// Events sent from background threads to the winit event loop.
#[derive(Debug)]
enum UserEvent {
    /// New bytes are available from a pane's PTY; request a redraw.
    PtyOutput,
    /// Config file changed; apply new settings.
    ConfigChanged(Box<ConfigChanged>),
}

/// Color for focused pane border indicator (FOCUS from Codex 2031 palette).
const FOCUS_BORDER_COLOR: [f32; 4] = {
    let c = PaletteColor::FOCUS;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.8,
    ]
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

    /// Root of the pane layout tree.
    layout: Option<LayoutNode>,

    /// ID of the currently focused pane.
    focused_pane: Option<PaneId>,

    /// Proxy to wake the event loop from PTY reader threads.
    event_proxy: EventLoopProxy<UserEvent>,

    /// Current modifiers state from winit.
    modifiers: Modifiers,

    /// Trailing-edge resize debounce.
    pending_resize: Option<PhysicalSize<u32>>,
    last_resize_at: Option<Instant>,

    /// Current cursor position in physical pixels.
    cursor_position: Option<(f64, f64)>,

    /// Whether the left mouse button is currently held.
    mouse_left_held: bool,

    /// Pane where the current mouse drag started (for consistent drag routing).
    mouse_drag_pane: Option<PaneId>,

    /// Current loaded configuration.
    config: TherminalConfig,

    /// Config file watcher handle (kept alive).
    _config_watcher: Option<ConfigWatcher>,

    /// Last split direction used (for auto-direction alternation).
    last_split_direction: SplitDirection,
}

impl App {
    fn new(event_proxy: EventLoopProxy<UserEvent>) -> Self {
        let config = TherminalConfig::load();
        info!(
            font_size = config.font.size,
            title = %config.general.title,
            "loaded config"
        );

        let config_watcher = match ConfigWatcher::start() {
            Ok((watcher, rx)) => {
                let proxy = event_proxy.clone();
                thread::Builder::new()
                    .name("config-event-bridge".into())
                    .spawn(move || {
                        while let Ok(event) = rx.recv() {
                            if proxy
                                .send_event(UserEvent::ConfigChanged(Box::new(event)))
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .expect("failed to spawn config event bridge thread");
                Some(watcher)
            }
            Err(e) => {
                warn!(%e, "failed to start config watcher, hot-reload disabled");
                None
            }
        };

        Self {
            window: None,
            gpu: None,
            grid_renderer: None,
            layout: None,
            focused_pane: None,
            event_proxy,
            modifiers: Modifiers::default(),
            pending_resize: None,
            last_resize_at: None,
            cursor_position: None,
            mouse_left_held: false,
            mouse_drag_pane: None,
            config,
            _config_watcher: config_watcher,
            last_split_direction: SplitDirection::Horizontal,
        }
    }

    /// Initialize wgpu, grid renderer, and first pane.
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
        let mut font_config =
            FontConfig::new(self.config.font.family.clone(), self.config.font.size);
        font_config.fallback_families = self.config.font.extra_fallbacks.clone();
        font_config.font_size *= scale;
        font_config.line_height = font_config.font_size * self.config.font.line_height_scale;
        info!(
            scale,
            font_size = font_config.font_size,
            "Applying DPI scale to font"
        );
        let padding = self.config.general.padding;
        let mut grid_renderer = GridRenderer::new(
            &device,
            &queue,
            format,
            config.width,
            config.height,
            font_config,
            padding,
        );
        grid_renderer.apply_color_overrides(&self.config.colors);

        // ── First pane (fills entire window) ─────────────────────────────
        let full_rect = Rect::new(0.0, 0.0, config.width as f32, config.height as f32);
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = self.build_spawn_options();
        let proxy = self.event_proxy.clone();
        let pane = match crate::pane::spawn_pane(
            full_rect,
            &grid_renderer,
            scrollback,
            interceptor_cfg,
            scan_interval_secs,
            &spawn_options,
            |_pane_id| {
                let p = proxy.clone();
                Box::new(move || {
                    let _ = p.send_event(UserEvent::PtyOutput);
                })
            },
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "failed to spawn initial pane");
                return;
            }
        };
        let pane_id = pane.id;
        let layout = LayoutNode::Leaf(pane);

        let init_width = config.width;
        let init_height = config.height;

        self.window = Some(window);
        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
        });
        self.grid_renderer = Some(grid_renderer);
        self.layout = Some(layout);
        self.focused_pane = Some(pane_id);

        let (cols, rows) = self
            .grid_renderer
            .as_ref()
            .map(|r| r.grid_size(init_width, init_height))
            .unwrap_or((80, 24));
        info!("Initial pane {pane_id}: {cols}x{rows}");
    }

    /// Resize the surface and all panes.
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

        if let Some(renderer) = self.grid_renderer.as_mut() {
            renderer.resize(&gpu.device, &gpu.queue, new_size.width, new_size.height);
        }

        // Recalculate layout tree and resize all pane PTYs.
        let full_rect = Rect::new(0.0, 0.0, new_size.width as f32, new_size.height as f32);
        if let (Some(layout), Some(renderer)) = (self.layout.as_mut(), self.grid_renderer.as_ref())
        {
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);
        }

        debug!(
            "Resized to {}x{} ({} panes)",
            new_size.width,
            new_size.height,
            self.layout.as_ref().map_or(0, |l| l.pane_count()),
        );
    }

    /// Render a frame: render all panes and separators.
    fn render(&mut self) {
        let gpu = match self.gpu.as_ref() {
            Some(g) => g,
            None => return,
        };
        let renderer = match self.grid_renderer.as_mut() {
            Some(r) => r,
            None => return,
        };
        let layout = match self.layout.as_ref() {
            Some(l) => l,
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

        // Clear to background color (respects config overrides).
        let resolved_bg = renderer.resolved_bg();
        let clear_color = wgpu::Color {
            r: resolved_bg[0] as f64,
            g: resolved_bg[1] as f64,
            b: resolved_bg[2] as f64,
            a: resolved_bg[3] as f64,
        };
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        // Render each pane.
        let focused = self.focused_pane;
        let pane_count = layout.pane_count();
        let show_focus = pane_count > 1;

        render_panes_recursive(
            layout,
            focused,
            show_focus,
            renderer,
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

    /// Check if this key event is a pane management shortcut (Ctrl+Shift+...).
    /// Returns true if the event was consumed.
    fn handle_pane_shortcut(&mut self, key_event: &KeyEvent) -> bool {
        let state = self.modifiers.state();
        if !state.control_key() || !state.shift_key() {
            return false;
        }

        match &key_event.logical_key {
            // Ctrl+Shift+Enter -> auto-direction split
            Key::Named(NamedKey::Enter) => {
                self.split_focused_pane_auto();
                true
            }
            // Ctrl+Shift+H -> horizontal split
            Key::Character(s) if s.as_str() == "H" => {
                self.split_focused_pane(SplitDirection::Horizontal);
                true
            }
            // Ctrl+Shift+V -> vertical split (note: may conflict with paste on some systems)
            Key::Character(s) if s.as_str() == "V" => {
                self.split_focused_pane(SplitDirection::Vertical);
                true
            }
            // Ctrl+Shift+W -> close focused pane
            Key::Character(s) if s.as_str() == "W" => {
                self.close_focused_pane();
                true
            }
            // Ctrl+Shift+= -> grow ratio
            Key::Character(s) if s.as_str() == "+" || s.as_str() == "=" => {
                self.adjust_focused_ratio(0.05);
                true
            }
            // Ctrl+Shift+- -> shrink ratio
            Key::Character(s) if s.as_str() == "_" || s.as_str() == "-" => {
                self.adjust_focused_ratio(-0.05);
                true
            }
            // Ctrl+Shift+Arrow -> move focus
            Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::ArrowDown) => {
                self.move_focus(FocusDirection::Next);
                true
            }
            Key::Named(NamedKey::ArrowLeft) | Key::Named(NamedKey::ArrowUp) => {
                self.move_focus(FocusDirection::Prev);
                true
            }
            _ => false,
        }
    }

    /// Split the currently focused pane with auto-detected direction.
    fn split_focused_pane_auto(&mut self) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(focused) {
            Some(p) => p,
            None => return,
        };
        let fallback = match self.last_split_direction {
            SplitDirection::Horizontal => SplitDirection::Vertical,
            SplitDirection::Vertical => SplitDirection::Horizontal,
        };
        let direction = LayoutNode::auto_split_direction(pane.viewport, fallback);
        self.split_focused_pane(direction);
    }

    /// Split the currently focused pane.
    fn split_focused_pane(&mut self, direction: SplitDirection) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
        };
        let proxy = self.event_proxy.clone();

        let new_id = layout.split_pane(
            focused,
            direction,
            |viewport| match crate::pane::spawn_pane(
                viewport,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                |_pane_id| {
                    let p = proxy.clone();
                    Box::new(move || {
                        let _ = p.send_event(UserEvent::PtyOutput);
                    })
                },
            ) {
                Ok(pane) => Some(pane),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to spawn pane for split");
                    None
                }
            },
        );

        if let Some(new_id) = new_id {
            info!("Split pane {focused} {:?} -> new pane {new_id}", direction);
            self.last_split_direction = direction;

            // Resize all panes after split.
            let gpu = self.gpu.as_ref().unwrap();
            let full_rect = Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
            let layout = self.layout.as_mut().unwrap();
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            // Focus the new pane.
            self.focused_pane = Some(new_id);
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Close the currently focused pane.
    fn close_focused_pane(&mut self) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };

        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };

        match layout.remove_pane(focused) {
            None => {
                // Last pane -- close the window.
                info!("Last pane closed, exiting");
                // We can't exit from here directly, but we can request the window close.
                // The next event loop iteration will handle CloseRequested.
                // Signal exit: layout=None causes exit at next RedrawRequested.
                self.focused_pane = None;
                self.layout = None;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(true) => {
                info!("Closed pane {focused}");
                // Move focus to first available pane.
                let layout = self.layout.as_mut().unwrap();
                let ids = layout.pane_ids();
                self.focused_pane = ids.first().copied();

                // Relayout.
                let gpu = self.gpu.as_ref().unwrap();
                let full_rect =
                    Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
                let renderer = self.grid_renderer.as_ref().unwrap();
                layout.layout(full_rect);
                layout.resize_all_panes(renderer);

                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Some(false) => {
                // Pane not found (shouldn't happen).
                warn!("Focused pane {focused} not found in layout");
            }
        }
    }

    /// Move focus to the next or previous pane.
    fn move_focus(&mut self, direction: FocusDirection) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return,
        };

        if let Some(new_id) = layout.adjacent_pane(focused, direction) {
            self.focused_pane = Some(new_id);
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Adjust the split ratio around the focused pane.
    fn adjust_focused_ratio(&mut self, delta: f32) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };

        if layout.adjust_ratio(focused, delta) {
            // Relayout.
            let gpu = self.gpu.as_ref().unwrap();
            let full_rect = Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
            let renderer = self.grid_renderer.as_ref().unwrap();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);

            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Handle a keyboard event: encode it and write to the focused pane's PTY.
    fn handle_key_input(&mut self, key_event: &KeyEvent) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane_mut(focused) {
            Some(p) => p,
            None => return,
        };

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
            Key::Character(s) => s.chars().next().map(KeyCode::Char),
            _ => None,
        };

        let key_code = match key_code {
            Some(k) => k,
            None => return,
        };

        let state = self.modifiers.state();
        let mods = InputModifiers {
            ctrl: state.control_key(),
            alt: state.alt_key(),
            shift: state.shift_key(),
        };

        if let Some(bytes) = input::encode_key(&key_code, &mods) {
            if let Err(e) = pane.pty_writer.write_all(&bytes) {
                warn!("Failed to write to pane {} PTY: {e}", pane.id);
            }
        }
    }

    /// Convert physical pixel coordinates to terminal grid (col, row) for the focused pane.
    #[allow(dead_code)]
    fn pixel_to_grid(&self, px: f64, py: f64) -> Option<(usize, usize)> {
        let focused = self.focused_pane?;
        self.pixel_to_grid_for_pane(px, py, focused)
    }

    /// Convert physical pixel coordinates to terminal grid (col, row) for a specific pane.
    fn pixel_to_grid_for_pane(&self, px: f64, py: f64, pane_id: PaneId) -> Option<(usize, usize)> {
        let renderer = self.grid_renderer.as_ref()?;
        let layout = self.layout.as_ref()?;
        let pane = layout.find_pane(pane_id)?;

        let vp = pane.viewport;
        let col = ((px as f32 - vp.x() - renderer.padding_x()) / renderer.cell_width).floor();
        let row = ((py as f32 - vp.y() - renderer.padding_y()) / renderer.cell_height).floor();
        if col < 0.0 || row < 0.0 {
            return None;
        }
        let col = col as usize;
        let row = row as usize;

        let term_guard = pane.term.lock();
        let max_col = term_guard.columns().saturating_sub(1);
        let max_row = term_guard.screen_lines().saturating_sub(1);
        Some((col.min(max_col), row.min(max_row)))
    }

    /// Get the current terminal mode flags from the focused pane.
    #[allow(dead_code)]
    fn term_mode(&self) -> TermMode {
        self.focused_term_mode()
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

    /// Write bytes to the focused pane's PTY.
    #[allow(dead_code)]
    fn pty_write(&mut self, bytes: &[u8]) {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return,
        };
        self.pty_write_to_pane(bytes, focused);
    }

    /// Write bytes to a specific pane's PTY.
    fn pty_write_to_pane(&mut self, bytes: &[u8], pane_id: PaneId) {
        let layout = match self.layout.as_mut() {
            Some(l) => l,
            None => return,
        };
        if let Some(pane) = layout.find_pane_mut(pane_id) {
            if let Err(e) = pane.pty_writer.write_all(bytes) {
                warn!("Failed to write to pane {} PTY: {e}", pane.id);
            }
        }
    }

    /// Handle mouse scroll -- routes to the pane under the pointer.
    fn handle_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        // Resolve the hovered pane from cursor position.
        let (px, py) = match self.cursor_position {
            Some(pos) => pos,
            None => return,
        };
        let target_pane = match self.pane_at_position(px, py) {
            Some(id) => id,
            None => return,
        };

        let (col, row) = self
            .pixel_to_grid_for_pane(px, py, target_pane)
            .unwrap_or((0, 0));

        let lines = match delta {
            MouseScrollDelta::LineDelta(_x, y) => y,
            MouseScrollDelta::PixelDelta(pos) => {
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

        let mode = self.pane_term_mode(target_pane);
        let mouse_mode = mode.contains(TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(TermMode::MOUSE_DRAG)
            || mode.contains(TermMode::MOUSE_MOTION);

        if mouse_mode {
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
            self.pty_write_to_pane(&seq, target_pane);
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let steps = lines.abs().ceil() as usize;
            let arrow = if lines > 0.0 { b"\x1b[A" } else { b"\x1b[B" };
            let mut seq = Vec::with_capacity(steps * 3);
            for _ in 0..steps {
                seq.extend_from_slice(arrow);
            }
            self.pty_write_to_pane(&seq, target_pane);
        } else {
            // Normal scrollback -- scroll the hovered pane.
            if let Some(layout) = self.layout.as_ref() {
                if let Some(pane) = layout.find_pane(target_pane) {
                    let scroll_lines = (lines * 3.0).round() as i32;
                    let mut term_guard = pane.term.lock();
                    term_guard.scroll_display(Scroll::Delta(scroll_lines));
                }
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Get focused pane's TermMode.
    fn focused_term_mode(&self) -> TermMode {
        let focused = match self.focused_pane {
            Some(id) => id,
            None => return TermMode::empty(),
        };
        self.pane_term_mode(focused)
    }

    /// Get a specific pane's TermMode.
    fn pane_term_mode(&self, pane_id: PaneId) -> TermMode {
        let layout = match self.layout.as_ref() {
            Some(l) => l,
            None => return TermMode::empty(),
        };
        match layout.find_pane(pane_id) {
            Some(pane) => *pane.term.lock().mode(),
            None => TermMode::empty(),
        }
    }

    /// Handle mouse button press/release -- routes to the pane under the pointer.
    fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        let (px, py) = match self.cursor_position {
            Some(pos) => pos,
            None => return,
        };

        // For button release during a drag, route to the pane where the drag
        // started so mouse-reporting apps don't get mismatched press/release.
        let target_pane = if state == ElementState::Released && button == MouseButton::Left {
            self.mouse_drag_pane
                .or_else(|| self.pane_at_position(px, py))
        } else {
            self.pane_at_position(px, py)
        };
        let target_pane = match target_pane {
            Some(id) => id,
            None => return,
        };

        let (col, row) = match self.pixel_to_grid_for_pane(px, py, target_pane) {
            Some(pos) => pos,
            None => return,
        };

        if button == MouseButton::Left {
            self.mouse_left_held = state == ElementState::Pressed;
            if state == ElementState::Pressed {
                self.mouse_drag_pane = Some(target_pane);
            } else {
                self.mouse_drag_pane = None;
            }
        }

        let mode = self.pane_term_mode(target_pane);
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
        self.pty_write_to_pane(&bytes, target_pane);
    }

    /// Handle mouse motion -- routes drag events to the pane where the drag started,
    /// and motion events to the pane under the pointer.
    fn handle_cursor_moved(&mut self, position: winit::dpi::PhysicalPosition<f64>) {
        let (px, py) = (position.x, position.y);
        self.cursor_position = Some((px, py));

        if self.mouse_left_held {
            // During a drag, route to the pane where the drag started so that
            // selections and drag reporting stay consistent even if the pointer
            // crosses a pane boundary.
            let target = match self.mouse_drag_pane {
                Some(id) => id,
                None => return,
            };

            let (col, row) = match self.pixel_to_grid_for_pane(px, py, target) {
                Some(pos) => pos,
                None => return,
            };

            let mode = self.pane_term_mode(target);
            if mode.contains(TermMode::MOUSE_DRAG) || mode.contains(TermMode::MOUSE_MOTION) {
                let mods = self.input_mods();
                let bytes = input::encode_mouse_drag(InputMouseButton::Left, col, row, &mods);
                self.pty_write_to_pane(&bytes, target);
            }
        } else {
            // No button held -- route motion to the pane under the pointer.
            let target = match self.pane_at_position(px, py) {
                Some(id) => id,
                None => return,
            };

            let (col, row) = match self.pixel_to_grid_for_pane(px, py, target) {
                Some(pos) => pos,
                None => return,
            };

            let mode = self.pane_term_mode(target);
            if mode.contains(TermMode::MOUSE_MOTION) {
                let mods = self.input_mods();
                let bytes = input::encode_mouse_motion(col, row, &mods);
                self.pty_write_to_pane(&bytes, target);
            }
        }
    }

    /// Apply a new configuration.
    fn apply_config(&mut self, new_config: TherminalConfig) {
        let old_config = std::mem::replace(&mut self.config, new_config);

        if self.config.general.title != old_config.general.title {
            if let Some(w) = self.window.as_ref() {
                w.set_title(&self.config.general.title);
            }
        }

        let font_changed = self.config.font.family != old_config.font.family
            || (self.config.font.size - old_config.font.size).abs() > f32::EPSILON
            || (self.config.font.line_height_scale - old_config.font.line_height_scale).abs()
                > f32::EPSILON;

        // ── Padding hot-reload ───────────────────────────────────────────
        let padding_changed =
            (self.config.general.padding - old_config.general.padding).abs() > f32::EPSILON;

        // ── Color overrides hot-reload ──────────────────────────────────
        let colors_changed = self.config.colors.background != old_config.colors.background
            || self.config.colors.foreground != old_config.colors.foreground
            || self.config.colors.cursor != old_config.colors.cursor
            || self.config.colors.selection != old_config.colors.selection;

        if colors_changed {
            if let Some(renderer) = self.grid_renderer.as_mut() {
                renderer.apply_color_overrides(&self.config.colors);
                info!("color overrides updated via hot-reload");
            }
        }

        let needs_relayout = font_changed || padding_changed;

        if needs_relayout {
            if let (Some(renderer), Some(gpu), Some(window)) = (
                self.grid_renderer.as_mut(),
                self.gpu.as_ref(),
                self.window.as_ref(),
            ) {
                if padding_changed {
                    renderer.set_padding(self.config.general.padding);
                    info!(
                        padding = self.config.general.padding,
                        "padding updated via hot-reload"
                    );
                }

                if font_changed {
                    let scale = window.scale_factor() as f32;
                    let mut new_font_config = FontConfig::new(
                        self.config.font.family.clone(),
                        self.config.font.size * scale,
                    );
                    new_font_config.fallback_families = self.config.font.extra_fallbacks.clone();
                    new_font_config.line_height =
                        self.config.font.size * self.config.font.line_height_scale * scale;
                    renderer.update_font(
                        new_font_config,
                        &gpu.device,
                        &gpu.queue,
                        gpu.config.width,
                        gpu.config.height,
                    );

                    info!(
                        font_size = self.config.font.size,
                        family = %self.config.font.family,
                        "font config updated via hot-reload"
                    );
                }

                // Resize all panes after font or padding change.
                let full_rect =
                    Rect::new(0.0, 0.0, gpu.config.width as f32, gpu.config.height as f32);
                if let Some(layout) = self.layout.as_mut() {
                    layout.layout(full_rect);
                    layout.resize_all_panes(renderer);
                }
            }
        }

        // ── Non-hot-reloadable settings (log a note) ────────────────────
        if self.config.general.shell != old_config.general.shell {
            info!(
                new_shell = %self.config.general.shell,
                "shell config changed; takes effect on next PTY spawn (restart needed)"
            );
        }
        if self.config.general.scrollback_lines != old_config.general.scrollback_lines {
            info!(
                new_scrollback = self.config.general.scrollback_lines,
                "scrollback_lines changed; takes effect on next PTY spawn (restart needed)"
            );
        }
        if self.config.general.env != old_config.general.env {
            info!("env config changed; takes effect on next PTY spawn (restart needed)");
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Build PTY spawn options from the current config (shell override + env).
    fn build_spawn_options(&self) -> therminal_terminal::pty::SpawnOptions {
        therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
        }
    }

    /// Find which pane contains the given physical pixel coordinates.
    fn pane_at_position(&self, px: f64, py: f64) -> Option<PaneId> {
        let layout = self.layout.as_ref()?;
        self.find_pane_at(layout, px as f32, py as f32)
    }

    fn find_pane_at(&self, node: &LayoutNode, px: f32, py: f32) -> Option<PaneId> {
        use therminal_core::geometry::Point;
        match node {
            LayoutNode::Leaf(pane) => {
                if pane.viewport.contains(Point::new(px, py)) {
                    Some(pane.id)
                } else {
                    None
                }
            }
            LayoutNode::Split { first, second, .. } => self
                .find_pane_at(first, px, py)
                .or_else(|| self.find_pane_at(second, px, py)),
            LayoutNode::Empty => None,
        }
    }
}

// ── Free functions for multi-pane rendering (avoids &self borrow conflicts) ──

/// Recursively render all panes in the layout tree.
#[allow(clippy::too_many_arguments)]
fn render_panes_recursive(
    node: &LayoutNode,
    focused: Option<PaneId>,
    show_focus: bool,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    match node {
        LayoutNode::Leaf(pane) => {
            render_single_pane(
                pane,
                focused == Some(pane.id) && show_focus,
                renderer,
                device,
                queue,
                encoder,
                view,
                surface_width,
                surface_height,
            );
        }
        LayoutNode::Split { first, second, .. } => {
            render_panes_recursive(
                first,
                focused,
                show_focus,
                renderer,
                device,
                queue,
                encoder,
                view,
                surface_width,
                surface_height,
            );
            render_panes_recursive(
                second,
                focused,
                show_focus,
                renderer,
                device,
                queue,
                encoder,
                view,
                surface_width,
                surface_height,
            );
        }
        LayoutNode::Empty => {}
    }
}

/// Render a single pane within its viewport rect.
#[allow(clippy::too_many_arguments)]
fn render_single_pane(
    pane: &PaneState,
    draw_focus_border: bool,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    let vp = pane.viewport;
    let mut term_guard = pane.term.lock();

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
            Some(set)
        }
    };

    let content = term_guard.renderable_content();
    let screen_lines = term_guard.screen_lines();
    let display_offset = content.display_offset;
    let cursor = content.cursor;
    let selection_range = content.selection;

    let cells: Vec<RenderCell> = content
        .display_iter
        .filter_map(|indexed| {
            let point = indexed.point;
            let cell = indexed.cell;

            let viewport_line = point.line.0 + display_offset as i32;
            let row = usize::try_from(viewport_line).ok()?;
            if row >= screen_lines {
                return None;
            }

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

    term_guard.reset_damage();
    drop(term_guard);

    // Clear per-pane caches so stale state from a previous pane doesn't bleed through.
    renderer.reset_pane_caches();

    // Temporarily adjust renderer's padding to offset by the pane's viewport origin.
    // Use a closure to guarantee restoration even if render() panics.
    let internal_pad_x = renderer.padding_x();
    let internal_pad_y = renderer.padding_y();
    renderer.set_viewport_offset(vp.x() + internal_pad_x, vp.y() + internal_pad_y);

    renderer.render(
        &cells,
        &cursor,
        screen_lines,
        selection_range.as_ref(),
        display_offset,
        damaged_rows.as_ref(),
        device,
        queue,
        encoder,
        view,
        surface_width,
        surface_height,
    );

    renderer.restore_padding();

    // Draw focus indicator border for the focused pane.
    if draw_focus_border {
        draw_pane_focus_border(
            pane,
            renderer,
            device,
            encoder,
            view,
            surface_width,
            surface_height,
        );
    }
}

/// Draw a subtle border around the focused pane.
fn draw_pane_focus_border(
    pane: &PaneState,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let vp = pane.viewport;
    let t = 2.0_f32; // border thickness
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let mut verts: Vec<ColorVertex> = Vec::new();

    // Top edge
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        vp.width(),
        t,
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    // Bottom edge
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.bottom() - t,
        vp.width(),
        t,
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    // Left edge
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        t,
        vp.height(),
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));
    // Right edge
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.right() - t,
        vp.y(),
        t,
        vp.height(),
        sw,
        sh,
        FOCUS_BORDER_COLOR,
    ));

    if verts.is_empty() {
        return;
    }

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("focus_border_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("focus_border_pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });

    pass.set_pipeline(&renderer.rect_pipeline);
    pass.set_vertex_buffer(0, vertex_buf.slice(..));
    pass.draw(0..verts.len() as u32, 0..1);
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title(&self.config.general.title)
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.general.window_width,
                self.config.general.window_height,
            ));

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
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            UserEvent::ConfigChanged(changed) => {
                info!("applying config change (hot-reload)");
                self.apply_config(changed.config.clone());
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
                if let Some(size) = self.pending_resize.take() {
                    self.last_resize_at = Some(Instant::now());
                    self.resize(size);
                }

                // If layout was removed (last pane closed), exit.
                if self.layout.is_none() {
                    event_loop.exit();
                    return;
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
                // Focus-follows-click: if clicking in a different pane, switch focus.
                if state == ElementState::Pressed && button == MouseButton::Left {
                    if let Some((px, py)) = self.cursor_position {
                        if let Some(pane_id) = self.pane_at_position(px, py) {
                            if self.focused_pane != Some(pane_id) {
                                self.focused_pane = Some(pane_id);
                                if let Some(w) = self.window.as_ref() {
                                    w.request_redraw();
                                }
                            }
                        }
                    }
                }
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
                // Check pane management shortcuts first.
                if !self.handle_pane_shortcut(key_event) {
                    self.handle_key_input(key_event);
                }
            }

            _ => {}
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
