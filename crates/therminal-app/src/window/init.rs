//! App initialization: construction and GPU/first-pane setup.
//!
//! Split out from `mod.rs` to keep the coordinator small. These methods are
//! only called once per `App` instance, so isolating them clarifies the hot
//! path in `event_handler` and `render_driver`.

use std::sync::Arc;
use std::thread;

use tracing::{info, warn};
use winit::dpi::PhysicalSize;
use winit::event_loop::EventLoopProxy;
use winit::window::Window;

use crate::grid_renderer::{FontConfig, GridRenderer};
use crate::pane::auto_tile::SwarmDebouncer;
use crate::pane::{AutoTileDebouncer, LayoutNode, WorkspaceManager};
use therminal_core::config::TherminalConfig;
use therminal_core::config_watcher::ConfigWatcher;
use therminal_core::font::PLATFORM_MONOSPACE;
use therminal_core::geometry::Rect;
use therminal_terminal::interceptor::InterceptorConfig;

use super::keybindings::build_binding_map;
use super::{App, GpuState, NotificationSource, UserEvent, chrome};

impl App {
    pub(super) fn new(event_proxy: EventLoopProxy<UserEvent>) -> Self {
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

        // Create the shared agent registry and auto-tile debouncer.
        let mut agent_registry = therminal_terminal::agent_registry::AgentRegistry::new();
        let auto_tile_debouncer = if config.general.auto_tile {
            agent_registry
                .take_event_rx()
                .map(|rx| AutoTileDebouncer::new(rx, config.general.auto_tile_debounce_ms))
        } else {
            // Consume the receiver so it doesn't fill up, even if auto-tile is off.
            let _ = agent_registry.take_event_rx();
            None
        };

        // Start agent notification listener thread if agent_waiting is enabled.
        if config.notifications.agent_waiting {
            if let Some(notification_rx) = agent_registry.take_notification_rx() {
                let proxy = event_proxy.clone();
                thread::Builder::new()
                    .name("agent-notify".into())
                    .spawn(move || {
                        use therminal_terminal::agent_registry::{AgentEvent, AgentStatus};
                        while let Ok(event) = notification_rx.recv() {
                            if let AgentEvent::StatusChanged {
                                new_status: AgentStatus::AwaitingInput,
                                pane_id,
                                ..
                            } = event
                            {
                                let _ = proxy.send_event(UserEvent::DesktopNotification {
                                    title: "Agent waiting".to_string(),
                                    body: format!("Agent in pane {pane_id} is awaiting input"),
                                    source: NotificationSource::Agent,
                                });
                            }
                        }
                    })
                    .expect("failed to spawn agent notification thread");
            }
        } else {
            // Consume the receiver so it doesn't fill up.
            let _ = agent_registry.take_notification_rx();
        }

        let agent_registry = Arc::new(std::sync::Mutex::new(agent_registry));

        // Start the swarm watcher: a background thread that polls
        // ~/.claude/projects/*/*/subagents/agent-*.jsonl and emits spawn/reclaim
        // events when Claude subagents start and finish. Gated on the same
        // `auto_tile` flag as process-tree auto-tiling.
        // Start the swarm watcher: a background thread that polls
        // ~/.claude/projects/*/*/subagents/agent-*.jsonl and emits spawn/reclaim
        // events when Claude subagents start and finish. Gated on the same
        // `auto_tile` flag as process-tree auto-tiling.
        //
        // Events flow: swarm_watcher thread -> bridge thread -> SwarmDebouncer
        // channel. The bridge also sends a tiny wake `UserEvent` so the winit
        // loop polls the debouncer; the debouncer applies a 1.5s window so
        // spawn-then-reclaim cycles cancel before any pane is created.
        let swarm_debouncer = if config.general.auto_tile {
            let raw_rx = crate::pane::swarm_watcher::spawn();
            let (deb_tx, deb_rx) =
                std::sync::mpsc::channel::<crate::pane::swarm_watcher::SwarmWatcherEvent>();
            let proxy = event_proxy.clone();
            thread::Builder::new()
                .name("swarm-watcher-bridge".into())
                .spawn(move || {
                    while let Ok(event) = raw_rx.recv() {
                        if deb_tx.send(event).is_err() {
                            break;
                        }
                        // Wake the event loop so it polls the debouncer.
                        if proxy
                            .send_event(super::UserEvent::SwarmWatcherTick)
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("failed to spawn swarm watcher bridge");
            Some(SwarmDebouncer::new(deb_rx, 1500))
        } else {
            None
        };

        Self {
            window: None,
            gpu: None,
            grid_renderer: None,
            workspaces: None,
            agent_registry,
            event_proxy,
            modifiers: Default::default(),
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
            last_split_direction: crate::pane::SplitDirection::Horizontal,
            show_help_overlay: false,
            active_menu: None,
            separator_drag: None,
            separator_cursor_active: false,
            hyperlink_cursor_active: false,
            edge_cursor_active: false,
            last_separator_click: None,
            last_tab_bar_click: None,
            last_close_action: None,
            auto_tile_debouncer,
            swarm_debouncer,
            swarm_panes: std::collections::HashMap::new(),
            visual_bell_start: None,
            zoomed_layout: None,
            status_bar_hit_areas: chrome::StatusBarHitAreas::default(),
            region_jump_toast: None,
        }
    }

    /// Initialize wgpu, grid renderer, and first pane.
    pub(super) fn init_gpu(&mut self, window: Arc<Window>) {
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
        let effective_family = if self.config.font.family.is_empty() {
            PLATFORM_MONOSPACE.to_string()
        } else {
            self.config.font.family.clone()
        };
        let mut font_config = FontConfig::new(effective_family, self.config.font.size);
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
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = self.build_spawn_options();
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        let pane = match crate::pane::spawn_pane(
            full_rect,
            &grid_renderer,
            scrollback,
            interceptor_cfg,
            scan_interval_secs,
            &spawn_options,
            registry,
            |pane_id| {
                let p1 = proxy.clone();
                let p2 = proxy.clone();
                let p3 = proxy.clone();
                let p4 = proxy.clone();
                crate::pane::PaneCallbacks {
                    wake: Box::new(move || {
                        let _ = p1.send_event(UserEvent::PtyOutput);
                    }),
                    on_exit: Box::new(move || {
                        let _ = p2.send_event(UserEvent::PaneExited(pane_id));
                    }),
                    on_bell: Box::new(move || {
                        let _ = p3.send_event(UserEvent::Bell(pane_id));
                    }),
                    on_notification: Box::new(move |text| {
                        let _ = p4.send_event(UserEvent::DesktopNotification {
                            title: "Therminal".to_string(),
                            body: text,
                            source: NotificationSource::Osc9,
                        });
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

    /// Build PTY spawn options from the current config (shell override + env).
    pub(super) fn build_spawn_options(&self) -> therminal_terminal::pty::SpawnOptions {
        therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        }
    }

    /// Resize the surface and all panes.
    pub(super) fn resize(&mut self, new_size: PhysicalSize<u32>) {
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

        tracing::debug!(
            "Resized to {}x{} ({} panes)",
            new_size.width,
            new_size.height,
            self.workspaces
                .as_ref()
                .map_or(0, |wm| wm.layout().pane_count()),
        );
    }
}
