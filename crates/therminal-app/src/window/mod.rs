//! Winit 0.30 window with wgpu surface for Therminal.
//!
//! Implements the full terminal pipeline with split-pane support:
//!   Keyboard (winit) -> encode_key() -> focused pane's PTY write
//!   PTY read -> vte::ansi::Processor -> Term -> damage
//!   Damage -> grid_renderer.render() per pane -> wgpu surface -> winit window
//!   Resize -> recalculate layout tree -> resize all pane PTYs + Terms
//!
//! Keyboard shortcuts are config-driven via `[keybindings]` in therminal.toml.
//! Default bindings (all Ctrl+Shift):
//!   H  -- split horizontal   D  -- split vertical   Enter -- auto split
//!   W  -- close pane         = -- grow ratio         - -- shrink ratio
//!   Arrows -- move focus     N/P -- focus next/prev  Z -- zoom pane
//!   C -- copy                V -- paste

mod chrome;
mod help_overlay;
mod keybindings;
mod mouse;
mod pane_ops;
mod render;

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, Modifiers, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use crate::grid_renderer::{FontConfig, GridRenderer};
use crate::menu::ContextMenu;
use crate::pane::{LayoutNode, PaneId, SplitDirection, WorkspaceManager};
use therminal_core::config::{KeyAction, TherminalConfig};
use therminal_core::config_watcher::{ConfigChanged, ConfigWatcher};
use therminal_core::geometry::Rect;
use therminal_terminal::input::{self, KeyCode, Modifiers as InputModifiers};
use therminal_terminal::interceptor::InterceptorConfig;

use keybindings::{BindingLookup, build_binding_map, lookup_binding};
use mouse::HeaderAction;

// ── Custom event for waking the event loop from the PTY reader ───────────

/// Events sent from background threads to the winit event loop.
#[derive(Debug)]
enum UserEvent {
    /// New bytes are available from a pane's PTY; request a redraw.
    PtyOutput,
    /// A pane's PTY has closed (shell exited); remove the pane.
    PaneExited(crate::pane::PaneId),
    /// Config file changed; apply new settings.
    ConfigChanged(Box<ConfigChanged>),
}

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

    /// Workspace manager holding all workspace layouts.
    workspaces: Option<WorkspaceManager>,

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

    /// Whether a mouse-driven selection is currently in progress (dragging).
    selection_in_progress: bool,

    /// Pane that owns the current selection (for multi-pane awareness).
    selection_pane: Option<PaneId>,

    /// Timestamp of the last left-click (for double/triple click detection).
    last_click_time: Option<Instant>,

    /// Position of the last left-click in grid coords (col, row).
    last_click_pos: Option<(usize, usize)>,

    /// Click count (1 = single, 2 = double/word, 3 = triple/line).
    click_count: u8,

    /// Current loaded configuration.
    config: TherminalConfig,

    /// Parsed keybinding lookup map (rebuilt on config reload).
    binding_map: HashMap<BindingLookup, KeyAction>,

    /// Config file watcher handle (kept alive).
    _config_watcher: Option<ConfigWatcher>,

    /// Last split direction used (for auto-direction alternation).
    last_split_direction: SplitDirection,

    /// Whether the keybinding help overlay is currently visible.
    show_help_overlay: bool,

    /// Active context menu, if one is open.
    active_menu: Option<ContextMenu>,

    /// Active separator drag state (path to split node, direction, parent rect).
    separator_drag: Option<SeparatorDrag>,

    /// Whether the cursor is currently showing a resize icon (for separator hover).
    separator_cursor_active: bool,

    /// Whether the cursor is currently showing a pointer icon (for hyperlink hover).
    hyperlink_cursor_active: bool,

    /// Timestamp of last separator click (for double-click detection).
    last_separator_click: Option<Instant>,

    /// Timestamp of last tab bar click (for CSD double-click-to-maximize).
    last_tab_bar_click: Option<Instant>,
}

/// State for an in-progress separator drag.
struct SeparatorDrag {
    /// Path to the split node being dragged (from `separator_hit_test`).
    path: Vec<bool>,
    /// Direction of the split being dragged.
    direction: SplitDirection,
    /// Bounding rect of the split node (for ratio computation).
    parent_rect: Rect,
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

        let binding_map = build_binding_map(&config);

        Self {
            window: None,
            gpu: None,
            grid_renderer: None,
            workspaces: None,
            event_proxy,
            modifiers: Modifiers::default(),
            pending_resize: None,
            last_resize_at: None,
            cursor_position: None,
            mouse_left_held: false,
            mouse_drag_pane: None,
            selection_in_progress: false,
            selection_pane: None,
            last_click_time: None,
            last_click_pos: None,
            click_count: 0,
            config,
            binding_map,
            _config_watcher: config_watcher,
            last_split_direction: SplitDirection::Horizontal,
            show_help_overlay: false,
            active_menu: None,
            separator_drag: None,
            separator_cursor_active: false,
            hyperlink_cursor_active: false,
            last_separator_click: None,
            last_tab_bar_click: None,
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
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
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

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("therminal"),
            ..Default::default()
        }))
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

        // ── First pane (fills window minus status bar and tab bar) ─────
        let status_bar_h =
            crate::pane::effective_status_bar_height(self.config.general.show_status_bar);
        let tab_bar_h = crate::pane::effective_tab_bar_height_csd(
            self.config.general.show_tab_bar,
            self.config.general.use_csd,
        );
        let full_rect = Rect::new(
            0.0,
            tab_bar_h,
            config.width as f32,
            config.height as f32 - status_bar_h - tab_bar_h,
        );
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
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
            |pane_id| {
                let p1 = proxy.clone();
                let p2 = proxy.clone();
                crate::pane::PaneCallbacks {
                    wake: Box::new(move || {
                        let _ = p1.send_event(UserEvent::PtyOutput);
                    }),
                    on_exit: Box::new(move || {
                        let _ = p2.send_event(UserEvent::PaneExited(pane_id));
                    }),
                }
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
        let wm = WorkspaceManager::new(layout, Some(pane_id));

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
        self.workspaces = Some(wm);

        // Resize initial pane with correct header height (0 for single pane).
        if let Some(wm) = self.workspaces.as_mut()
            && let Some(renderer) = self.grid_renderer.as_ref()
        {
            wm.layout_mut().resize_all_panes(renderer);
        }

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
        if let Some(full_rect) = self.compute_layout_rect()
            && let (Some(wm), Some(renderer)) =
                (self.workspaces.as_mut(), self.grid_renderer.as_ref())
        {
            let layout = wm.layout_mut();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer);
        }

        debug!(
            "Resized to {}x{} ({} panes)",
            new_size.width,
            new_size.height,
            self.workspaces
                .as_ref()
                .map_or(0, |wm| wm.layout().pane_count()),
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
        let layout = match self.workspaces.as_ref().map(|wm| wm.layout()) {
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
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        // Render each pane.
        let focused = self.workspaces.as_ref().and_then(|wm| wm.focused_pane());
        let pane_count = layout.pane_count();
        let show_focus = pane_count > 1;

        // Submit the clear pass immediately so pane renders can use fresh encoders.
        gpu.queue.submit(std::iter::once(encoder.finish()));

        // Clear hotspot/hyperlink maps once per frame so all panes can
        // contribute entries that persist until the next frame.
        renderer.clear_frame_maps();

        let mut pane_counter = 0;
        render::render_panes_recursive(
            layout,
            focused,
            show_focus,
            pane_count,
            &mut pane_counter,
            renderer,
            &gpu.device,
            &gpu.queue,
            &view,
            gpu.config.width,
            gpu.config.height,
        );

        // ── Status bar ──────────────────────────────────────────────────
        if self.config.general.show_status_bar {
            // Gather status info from the focused pane.
            let focused_pane = self
                .workspaces
                .as_ref()
                .and_then(|wm| wm.focused_pane())
                .and_then(|fid| layout.find_pane(fid));

            let (cwd, last_exit_code, agent_name, dimensions) = if let Some(pane) = focused_pane {
                let status = pane.status.lock().unwrap_or_else(|e| e.into_inner());
                let term_guard = pane.term.lock();
                let cols = alacritty_terminal::grid::Dimensions::columns(&*term_guard);
                let rows = alacritty_terminal::grid::Dimensions::screen_lines(&*term_guard);
                drop(term_guard);
                (
                    status.cwd.clone(),
                    status.last_exit_code,
                    status.agent_name.clone(),
                    (cols, rows),
                )
            } else {
                (None, None, None, (80, 24))
            };

            let (workspace_ids, active_workspace) = if let Some(wm) = self.workspaces.as_ref() {
                (wm.workspace_ids(), wm.active_id())
            } else {
                (vec![1], 1)
            };

            let status_info = chrome::StatusBarInfo {
                agent_name,
                cwd,
                dimensions,
                last_exit_code,
                show_agent_indicator: self.config.trust.show_agent_indicator,
                workspace_ids,
                active_workspace,
            };

            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("status_bar_encoder"),
                });
            chrome::draw_status_bar(
                &status_info,
                renderer,
                &gpu.device,
                &gpu.queue,
                &mut encoder,
                &view,
                gpu.config.width,
                gpu.config.height,
            );

            gpu.queue.submit(std::iter::once(encoder.finish()));
        }

        // ── Tab bar / CSD title bar ────────────────────────────────────
        let use_csd = self.config.general.use_csd;
        if self.config.general.show_tab_bar || use_csd {
            let (workspace_ids, active_workspace) = if let Some(wm) = self.workspaces.as_ref() {
                (wm.workspace_ids(), wm.active_id())
            } else {
                (vec![1], 1)
            };

            let tab_info = chrome::TabBarInfo {
                workspace_ids,
                active_workspace,
            };

            let bar_h = crate::pane::effective_tab_bar_height_csd(
                self.config.general.show_tab_bar,
                use_csd,
            );

            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tab_bar_encoder"),
                });
            chrome::draw_tab_bar(
                &tab_info,
                renderer,
                &gpu.device,
                &gpu.queue,
                &mut encoder,
                &view,
                gpu.config.width,
                gpu.config.height,
                bar_h,
                self.config.general.show_tab_bar,
            );

            // Draw CSD window control buttons on top of the tab bar.
            if use_csd {
                let hover_x = self
                    .cursor_position
                    .filter(|(_, py)| (*py as f32) < bar_h)
                    .map(|(px, _)| px as f32);
                chrome::draw_csd_buttons(
                    renderer,
                    &gpu.device,
                    &gpu.queue,
                    &mut encoder,
                    &view,
                    gpu.config.width,
                    gpu.config.height,
                    bar_h,
                    hover_x,
                );
            }

            gpu.queue.submit(std::iter::once(encoder.finish()));
        }

        // ── Help overlay (on top of everything) ─────────────────────────
        if self.show_help_overlay {
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("help_overlay_encoder"),
                });
            help_overlay::draw_help_overlay(
                &self.config.keybindings,
                renderer,
                &gpu.device,
                &gpu.queue,
                &mut encoder,
                &view,
                gpu.config.width,
                gpu.config.height,
            );
            gpu.queue.submit(std::iter::once(encoder.finish()));
        }

        // ── Context menu overlay (on top of everything) ────────────────
        if let Some(ref menu) = self.active_menu {
            crate::menu::render_context_menu(
                menu,
                renderer,
                &gpu.device,
                &gpu.queue,
                &view,
                gpu.config.width,
                gpu.config.height,
            );
        }

        output.present();
    }

    /// Check if this key event matches a configured keybinding.
    /// Returns true if the event was consumed.
    fn handle_keybinding(&mut self, key_event: &KeyEvent) -> bool {
        let action = match lookup_binding(&self.binding_map, &self.modifiers, key_event) {
            Some(a) => a,
            None => return false,
        };

        match action {
            KeyAction::SplitHorizontal => self.split_focused_pane(SplitDirection::Horizontal),
            KeyAction::SplitVertical => self.split_focused_pane(SplitDirection::Vertical),
            KeyAction::SplitAuto => self.split_focused_pane_auto(),
            KeyAction::ClosePane => self.close_focused_pane(),
            KeyAction::ResizeGrow => self.adjust_focused_ratio(0.05),
            KeyAction::ResizeShrink => self.adjust_focused_ratio(-0.05),
            KeyAction::FocusNext => {
                self.move_focus(crate::pane::FocusDirection::Next);
            }
            KeyAction::FocusPrev => {
                self.move_focus(crate::pane::FocusDirection::Prev);
            }
            KeyAction::FocusUp => {
                self.move_focus_spatial(crate::pane::SpatialDirection::Up);
            }
            KeyAction::FocusDown => {
                self.move_focus_spatial(crate::pane::SpatialDirection::Down);
            }
            KeyAction::FocusLeft => {
                self.move_focus_spatial(crate::pane::SpatialDirection::Left);
            }
            KeyAction::FocusRight => {
                self.move_focus_spatial(crate::pane::SpatialDirection::Right);
            }
            KeyAction::SwapNext => {
                self.swap_focused_pane(crate::pane::FocusDirection::Next);
            }
            KeyAction::SwapPrev => {
                self.swap_focused_pane(crate::pane::FocusDirection::Prev);
            }
            KeyAction::ZoomPane => {
                // TODO: implement pane zoom toggle (tn-oxa)
                info!("zoom pane: not yet implemented");
            }
            KeyAction::Copy => {
                self.copy_selection();
            }
            KeyAction::Paste => {
                self.paste_clipboard();
            }
            KeyAction::FontSizeUp => {
                self.adjust_font_size_action(1.0);
            }
            KeyAction::FontSizeDown => {
                self.adjust_font_size_action(-1.0);
            }
            KeyAction::FontSizeReset => {
                self.reset_font_size_action();
            }
            KeyAction::ShowHelp => {
                self.show_help_overlay = !self.show_help_overlay;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            KeyAction::CloseAllPanes => {
                self.close_all_panes();
            }
            KeyAction::RestoreLayout => {
                self.restore_layout();
            }
            KeyAction::SwitchWorkspace(n) => {
                self.switch_workspace(n);
            }
            KeyAction::SendToWorkspace(n) => {
                self.send_to_workspace(n);
            }
            // Hotspot actions are menu-only; they shouldn't reach keybinding dispatch.
            KeyAction::HotspotCopy(_)
            | KeyAction::HotspotOpenInEditor(_)
            | KeyAction::HotspotOpenExternal(_) => {}
        }
        true
    }

    /// Handle a keyboard event: encode it and write to the focused pane's PTY.
    fn handle_key_input(&mut self, key_event: &KeyEvent) {
        let focused = match self.workspaces.as_ref().and_then(|wm| wm.focused_pane()) {
            Some(id) => id,
            None => return,
        };
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
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
                NamedKey::Space => Some(KeyCode::Char(' ')),
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

        if let Some(bytes) = input::encode_key(&key_code, &mods)
            && let Err(e) = pane.pty_writer.write_all(&bytes)
        {
            warn!("Failed to write to pane {} PTY: {e}", pane.id);
        }
    }

    // ── Context menu ──────────────────────────────────────────────────

    /// Open a context menu at the given pixel position.
    fn open_context_menu(&mut self, px: f32, py: f32) {
        let pane_id = match self.pane_at_position(px as f64, py as f64) {
            Some(id) => id,
            None => return,
        };

        let bindings = &self.config.keybindings.bindings;

        // Check if the pane under the cursor has a selection.
        let has_selection = if let Some(layout) = self.get_layout() {
            if let Some(pane) = layout.find_pane(pane_id) {
                let term_guard = pane.term.lock();
                term_guard
                    .selection_to_string()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            } else {
                false
            }
        } else {
            false
        };

        let menu = if has_selection {
            let text = self
                .get_layout()
                .and_then(|l| l.find_pane(pane_id))
                .and_then(|p| p.term.lock().selection_to_string())
                .unwrap_or_default();
            crate::menu::build_selection_menu(text, bindings, (px, py))
        } else {
            crate::menu::build_pane_menu(pane_id, bindings, (px, py))
        };

        self.active_menu = Some(menu);
    }

    /// Execute the currently selected menu action and close the menu.
    fn execute_menu_action(&mut self) {
        let action = match self.active_menu.as_ref().and_then(|m| m.selected_action()) {
            Some(a) => a,
            None => {
                self.active_menu = None;
                return;
            }
        };
        self.active_menu = None;

        match action {
            KeyAction::SplitHorizontal => self.split_focused_pane(SplitDirection::Horizontal),
            KeyAction::SplitVertical => self.split_focused_pane(SplitDirection::Vertical),
            KeyAction::SplitAuto => self.split_focused_pane_auto(),
            KeyAction::ClosePane => self.close_focused_pane(),
            KeyAction::CloseAllPanes => self.close_all_panes(),
            KeyAction::RestoreLayout => self.restore_layout(),
            KeyAction::Copy => self.copy_selection(),
            KeyAction::Paste => self.paste_clipboard(),
            KeyAction::HotspotCopy(ref text) => {
                crate::clipboard::copy_to_clipboard(text);
            }
            KeyAction::HotspotOpenInEditor(ref text) => {
                self.open_in_editor(text);
            }
            KeyAction::HotspotOpenExternal(ref text) => {
                if let Err(e) = open::that(text) {
                    info!("failed to open externally {text}: {e}");
                }
            }
            _ => {
                info!("menu action {:?} not handled", action);
            }
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Apply a new configuration.
    fn apply_config(&mut self, new_config: TherminalConfig) {
        let old_config = std::mem::replace(&mut self.config, new_config);

        // ── Keybinding hot-reload ──────────────────────────────────────
        self.binding_map = build_binding_map(&self.config);
        info!(
            "keybinding map rebuilt ({} bindings)",
            self.binding_map.len()
        );

        if self.config.general.title != old_config.general.title
            && let Some(w) = self.window.as_ref()
        {
            w.set_title(&self.config.general.title);
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

        if colors_changed && let Some(renderer) = self.grid_renderer.as_mut() {
            renderer.apply_color_overrides(&self.config.colors);
            info!("color overrides updated via hot-reload");
        }

        let status_bar_changed =
            self.config.general.show_status_bar != old_config.general.show_status_bar;
        let tab_bar_changed = self.config.general.show_tab_bar != old_config.general.show_tab_bar;

        let needs_relayout =
            font_changed || padding_changed || status_bar_changed || tab_bar_changed;

        if needs_relayout
            && let (Some(renderer), Some(gpu), Some(window)) = (
                self.grid_renderer.as_mut(),
                self.gpu.as_ref(),
                self.window.as_ref(),
            )
        {
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
            let full_rect = crate::pane::content_area_rect_csd(
                gpu.config.width as f32,
                gpu.config.height as f32,
                self.config.general.show_status_bar,
                self.config.general.show_tab_bar,
                self.config.general.use_csd,
            );
            if let Some(wm) = self.workspaces.as_mut() {
                let layout = wm.layout_mut();
                layout.layout(full_rect);
                layout.resize_all_panes(renderer);
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

    /// Adjust font size by `delta` points, resize panes, and request a redraw.
    fn adjust_font_size_action(&mut self, delta: f32) {
        if let (Some(renderer), Some(gpu)) = (self.grid_renderer.as_mut(), self.gpu.as_ref()) {
            let new_size = renderer.adjust_font_size(delta);
            renderer.resize(&gpu.device, &gpu.queue, gpu.config.width, gpu.config.height);
            info!(font_size = new_size, "font size adjusted");
        }
        self.relayout_and_redraw();
    }

    /// Reset font size to startup default, resize panes, and request a redraw.
    fn reset_font_size_action(&mut self) {
        if let (Some(renderer), Some(gpu)) = (self.grid_renderer.as_mut(), self.gpu.as_ref()) {
            let new_size = renderer.reset_font_size();
            renderer.resize(&gpu.device, &gpu.queue, gpu.config.width, gpu.config.height);
            info!(font_size = new_size, "font size reset to default");
        }
        self.relayout_and_redraw();
    }

    /// Build PTY spawn options from the current config (shell override + env).
    fn build_spawn_options(&self) -> therminal_terminal::pty::SpawnOptions {
        therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
        }
    }

    // ── Workspace facade methods ─────────────────────────────────────────
    // Centralized accessors that replace the ws_layout!, ws_layout_mut!,
    // ws_focused!, and ws_set_focused! macros.

    /// Get a shared reference to the active workspace's layout tree.
    pub(crate) fn get_layout(&self) -> Option<&LayoutNode> {
        self.workspaces.as_ref().map(|wm| wm.layout())
    }

    /// Get a mutable reference to the active workspace's layout tree.
    pub(crate) fn get_layout_mut(&mut self) -> Option<&mut LayoutNode> {
        self.workspaces.as_mut().map(|wm| wm.layout_mut())
    }

    /// Get the focused pane ID in the active workspace.
    pub(crate) fn focused_pane(&self) -> Option<PaneId> {
        self.workspaces.as_ref().and_then(|wm| wm.focused_pane())
    }

    /// Set the focused pane ID in the active workspace.
    pub(crate) fn set_focused_pane(&mut self, id: Option<PaneId>) {
        if let Some(wm) = self.workspaces.as_mut() {
            wm.set_focused_pane(id);
        }
    }

    /// Compute the content area rect from GPU dimensions and config flags.
    /// Returns `None` if the GPU state is not yet initialized.
    pub(crate) fn compute_layout_rect(&self) -> Option<Rect> {
        let gpu = self.gpu.as_ref()?;
        Some(crate::pane::content_area_rect_csd(
            gpu.config.width as f32,
            gpu.config.height as f32,
            self.config.general.show_status_bar,
            self.config.general.show_tab_bar,
            self.config.general.use_csd,
        ))
    }

    /// Relayout the active workspace's tree and resize all pane PTYs,
    /// then request a window redraw. No-op if GPU, renderer, or layout
    /// is unavailable.
    pub(crate) fn relayout_and_redraw(&mut self) {
        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };
        if let Some(layout) = self.get_layout_mut() {
            layout.layout(full_rect);
        }
        // Separate borrow scope: layout_mut + renderer.
        if let (Some(wm), Some(renderer)) = (self.workspaces.as_mut(), self.grid_renderer.as_ref())
        {
            wm.layout_mut().resize_all_panes(renderer);
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Request a window redraw (convenience wrapper).
    pub(crate) fn request_redraw(&self) {
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

// ── ApplicationHandler impl ─────────────────────────────────────────────

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let use_csd = self.config.general.use_csd;
        let mut attrs = Window::default_attributes()
            .with_title(&self.config.general.title)
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.config.general.window_width,
                self.config.general.window_height,
            ));

        if use_csd {
            attrs = attrs.with_decorations(false);
        }

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
            UserEvent::PaneExited(pane_id) => {
                info!(pane_id, "pane PTY exited, closing pane");
                self.close_pane_by_id(pane_id);
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

                // workspaces == None means all panes are gone and no restore
                // is pending — exit the window.
                if self.workspaces.is_none() {
                    event_loop.exit();
                    return;
                }

                self.render();
            }

            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }

            WindowEvent::CursorMoved { position, .. } => {
                // Update cursor position for menu hover tracking.
                self.cursor_position = Some((position.x, position.y));
                if self.active_menu.is_some() {
                    if let Some(gpu) = self.gpu.as_ref() {
                        let menu = self.active_menu.as_mut().unwrap();
                        let geo = menu.geometry(gpu.config.width as f32, gpu.config.height as f32);
                        let hovered = menu.item_at_position(
                            position.x as f32,
                            position.y as f32,
                            geo.x,
                            geo.y,
                            geo.width,
                            geo.item_height,
                            geo.section_gap,
                        );
                        if hovered != menu.selected_index {
                            menu.selected_index = hovered;
                            if let Some(w) = self.window.as_ref() {
                                w.request_redraw();
                            }
                        }
                    }
                } else {
                    self.handle_cursor_moved(position);
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Ignore scroll when context menu is open.
                if self.active_menu.is_some() {
                    return;
                }
                self.handle_mouse_wheel(delta);
            }

            WindowEvent::MouseInput { state, button, .. } => {
                // Dismiss help overlay on any mouse click.
                if self.show_help_overlay
                    && state == ElementState::Pressed
                    && button == MouseButton::Left
                {
                    self.show_help_overlay = false;
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                    return;
                }

                // ── Context menu interception ──────────────────────────────
                if self.active_menu.is_some() && state == ElementState::Pressed {
                    if button == MouseButton::Left {
                        if let Some((px, py)) = self.cursor_position {
                            let menu = self.active_menu.as_ref().unwrap();
                            let gpu = self.gpu.as_ref().unwrap();
                            let geo =
                                menu.geometry(gpu.config.width as f32, gpu.config.height as f32);
                            if menu.contains_point(px as f32, py as f32, geo.width, geo.height) {
                                // Click inside menu -- select and execute.
                                if let Some(idx) = menu.item_at_position(
                                    px as f32,
                                    py as f32,
                                    geo.x,
                                    geo.y,
                                    geo.width,
                                    geo.item_height,
                                    geo.section_gap,
                                ) {
                                    self.active_menu.as_mut().unwrap().selected_index = Some(idx);
                                    self.execute_menu_action();
                                }
                            } else {
                                // Click outside menu -- close it.
                                self.active_menu = None;
                            }
                        } else {
                            self.active_menu = None;
                        }
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                        return;
                    }
                    // Right-click while menu is open: close and re-open at new position.
                    if button == MouseButton::Right {
                        self.active_menu = None;
                        // Fall through to open a new menu below.
                    }
                }

                // ── Right-click: open context menu ─────────────────────────
                if state == ElementState::Pressed
                    && button == MouseButton::Right
                    && let Some((px, py)) = self.cursor_position
                {
                    self.open_context_menu(px as f32, py as f32);
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                    return;
                }

                // ── Tab bar / CSD title bar click handling ─────────────────
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && let Some((px, py)) = self.cursor_position
                {
                    let use_csd = self.config.general.use_csd;
                    let show_tab_bar = self.config.general.show_tab_bar;
                    let tab_bar_h =
                        crate::pane::effective_tab_bar_height_csd(show_tab_bar, use_csd);
                    if (show_tab_bar || use_csd) && (py as f32) < tab_bar_h {
                        // CSD window control buttons (right side).
                        if use_csd {
                            let surface_w = self
                                .gpu
                                .as_ref()
                                .map(|g| g.config.width as f32)
                                .unwrap_or(0.0);
                            if let Some(action) =
                                chrome::csd_button_hit_test(px as f32, tab_bar_h, surface_w)
                            {
                                match action {
                                    chrome::CsdAction::Close => {
                                        event_loop.exit();
                                    }
                                    chrome::CsdAction::Maximize => {
                                        if let Some(w) = self.window.as_ref() {
                                            w.set_maximized(!w.is_maximized());
                                        }
                                    }
                                    chrome::CsdAction::Minimize => {
                                        if let Some(w) = self.window.as_ref() {
                                            w.set_minimized(true);
                                        }
                                    }
                                }
                                return;
                            }
                        }

                        // Tab click: switch workspace.
                        if show_tab_bar {
                            let workspace_ids = self
                                .workspaces
                                .as_ref()
                                .map(|wm| wm.workspace_ids())
                                .unwrap_or_default();
                            if let Some(ws_id) = chrome::tab_bar_hit_test(px as f32, &workspace_ids)
                            {
                                self.switch_workspace(ws_id as u8);
                                if let Some(w) = self.window.as_ref() {
                                    w.request_redraw();
                                }
                                return;
                            }
                        }

                        // CSD: double-click empty area toggles maximize.
                        if use_csd {
                            let now = Instant::now();
                            let is_double = self.last_tab_bar_click.is_some_and(|t| {
                                now.duration_since(t) < Duration::from_millis(300)
                            });
                            if is_double {
                                self.last_tab_bar_click = None;
                                if let Some(w) = self.window.as_ref() {
                                    w.set_maximized(!w.is_maximized());
                                }
                                return;
                            }
                            self.last_tab_bar_click = Some(now);

                            // Start window drag on empty tab bar area.
                            if let Some(w) = self.window.as_ref()
                                && let Err(e) = w.drag_window()
                            {
                                warn!("drag_window failed: {e}");
                            }
                            return;
                        }

                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                        return;
                    }
                }

                // ── Separator drag: release ends drag ──────────────────────
                if state == ElementState::Released
                    && button == MouseButton::Left
                    && self.separator_drag.is_some()
                {
                    self.end_separator_drag();
                    return;
                }

                // ── Separator drag: press starts drag or double-click resets ─
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && let Some((px, py)) = self.cursor_position
                {
                    // Double-click detection on separator.
                    let now = Instant::now();
                    let is_separator = self.separator_hit(px as f32, py as f32).is_some();
                    if is_separator {
                        let is_double = self
                            .last_separator_click
                            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(300));
                        if is_double {
                            self.last_separator_click = None;
                            self.try_separator_double_click(px as f32, py as f32);
                            return;
                        }
                        self.last_separator_click = Some(now);
                        if self.try_start_separator_drag(px as f32, py as f32) {
                            return;
                        }
                    } else {
                        self.last_separator_click = None;
                    }
                }

                // Header button click detection (only when multiple panes).
                let mut header_handled = false;
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && let Some((px, py)) = self.cursor_position
                    && let Some(action) = self.header_hit_test(px, py)
                {
                    header_handled = true;
                    match action {
                        HeaderAction::Focus(pane_id) => {
                            self.set_focused_pane(Some(pane_id));
                        }
                        HeaderAction::Close(pane_id) => {
                            self.close_pane_by_id(pane_id);
                        }
                        HeaderAction::SplitH(pane_id) => {
                            self.split_pane_by_id(pane_id, SplitDirection::Horizontal);
                        }
                        HeaderAction::SplitV(pane_id) => {
                            self.split_pane_by_id(pane_id, SplitDirection::Vertical);
                        }
                    }
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }

                if !header_handled {
                    // Focus-follows-click: if clicking in a different pane, switch focus.
                    if state == ElementState::Pressed
                        && button == MouseButton::Left
                        && let Some((px, py)) = self.cursor_position
                        && let Some(pane_id) = self.pane_at_position(px, py)
                        && self.focused_pane() != Some(pane_id)
                    {
                        self.set_focused_pane(Some(pane_id));
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                    }
                    self.handle_mouse_input(state, button);
                }
            }

            WindowEvent::KeyboardInput {
                event:
                    ref key_event @ KeyEvent {
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                // ── Context menu keyboard navigation ───────────────────
                if self.active_menu.is_some() {
                    match &key_event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.active_menu = None;
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            self.active_menu.as_mut().unwrap().move_up();
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            self.active_menu.as_mut().unwrap().move_down();
                        }
                        Key::Named(NamedKey::Enter) => {
                            self.execute_menu_action();
                        }
                        _ => {
                            // Any other key closes the menu.
                            self.active_menu = None;
                        }
                    }
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                    return;
                }

                // When help overlay is visible, any key dismisses it.
                if self.show_help_overlay {
                    self.show_help_overlay = false;
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                    return;
                }

                // Check configured keybindings first.
                if self.handle_keybinding(key_event) {
                    // Keybinding consumed the event. Copy/Paste preserve
                    // selection; other actions clear it.
                    let action = lookup_binding(&self.binding_map, &self.modifiers, key_event);
                    let preserves =
                        matches!(action, Some(KeyAction::Copy) | Some(KeyAction::Paste));
                    if !preserves {
                        self.clear_selection();
                    }
                } else {
                    // Regular keypress clears any active selection.
                    self.clear_selection();
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
