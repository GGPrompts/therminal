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

        // Spawn the Claude session cwd tracker (tn-ykxb). The background
        // thread polls `/tmp/claude-code-state/*.json` and keeps a
        // pid->session->cwd index the renderer reads when building
        // Claude tool-call hotspots for a pane.
        let claude_cwd = crate::claude_cwd::ClaudeCwdTracker::spawn();

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
        let swarm_pane_pids: Option<crate::pane::swarm_watcher::PanePidProvider> = if config
            .general
            .auto_tile
            && config.general.swarm_watch_scope == therminal_core::config::SwarmWatchScope::Current
        {
            Some(Arc::new(std::sync::Mutex::new(Vec::new())))
        } else {
            None
        };
        let swarm_debouncer = if config.general.auto_tile {
            let raw_rx = crate::pane::swarm_watcher::spawn(
                config.general.swarm_watch_scope,
                swarm_pane_pids.clone(),
            );
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

        let pattern_engine = if config.patterns.enabled {
            use therminal_terminal::semantic_patterns::{PatternEngine, PatternEngineConfig};
            Some(PatternEngine::new(PatternEngineConfig {
                enabled: true,
                user_pattern_dir: config.patterns.directory.clone(),
                shipped_pattern_dir: None,
                max_patterns: config.patterns.max_patterns,
                slow_pattern_threshold_us: config.patterns.slow_pattern_threshold_us,
                slow_strike_limit: config.patterns.slow_strike_limit,
            }))
        } else {
            None
        };

        // Build the agent timeline source from config before `config` is
        // moved into the struct (tn-x85k).
        let agent_timeline = {
            let tc = &config.widgets.agent_timeline;
            let mut tl = crate::widgets::agent_timeline::AgentTimelineSource::new(
                tc.max_entries,
                tc.height_px,
                tc.position,
            );
            tl.visible = tc.enabled;
            tl
        };

        // tn-fzr0: probe PATH for the configured git TUI tools and cache
        // the available subset. Refreshed on every config reload below.
        let discovered_git_tools =
            super::git_ref_open::discover_git_tools(&config.hotspots.git_tools);
        if !discovered_git_tools.is_empty() {
            info!(
                count = discovered_git_tools.len(),
                tools = ?discovered_git_tools,
                "discovered git TUI tools on PATH"
            );
        }

        Self {
            window: None,
            gpu: None,
            grid_renderer: None,
            workspaces: None,
            agent_registry,
            claude_cwd,
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
            last_press_pixel: None,
            click_count: 0,
            config,
            discovered_git_tools,
            binding_map,
            _config_watcher: config_watcher,
            last_split_direction: crate::pane::SplitDirection::Horizontal,
            overlay_mode: None,
            help_overlay_scroll_rows: 0,
            settings_overlay: super::settings_overlay::SettingsOverlayState::new(),
            trust_escalation: None,
            active_menu: None,
            tab_menu_workspace_id: None,
            rename_state: None,
            separator_drag: None,
            separator_cursor_active: false,
            hyperlink_cursor_active: false,
            edge_cursor_active: false,
            last_separator_click: None,
            last_tab_bar_click: None,
            last_close_action: None,
            auto_tile_debouncer,
            swarm_debouncer,
            swarm_pane_pids,
            swarm_panes: std::collections::HashMap::new(),
            visual_bell_start: None,
            zoomed_layout: None,
            status_bar_hit_areas: chrome::StatusBarHitAreas::default(),
            delegate_summary: chrome::DelegateSummaryState::new(),
            toast: None,
            daemon_client: None,
            daemon_runtime: None,
            pane_id_map: super::PaneIdMap::default(),
            daemon_session_id: None,
            pattern_engine,
            widget_renderer: None,
            widget_manager: crate::widgets::WidgetManager::new(),
            agent_timeline,
            initial_pane_pending: false,
            focus_mode: false,
            deferred_remote_spawn: None,
            scrollback_compact_countdown: 0,
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
        font_config.ui_font_family = self.config.font.ui_font_family.clone();
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
        grid_renderer.set_ui_text_scale(self.config.accessibility.ui_text_scale);

        // ── Widget pipeline (tn-npd) ─────────────────────────────────────
        // Tiny textured-quad pipeline shared by all pre-rasterized widgets
        // (agent status badge PoC today; context gauges, tool-call cards,
        // thinking indicators in follow-ups). Construction is cheap — a
        // single shader module, sampler, bind group layout, and pipeline.
        self.widget_renderer = Some(crate::widgets::WidgetRenderer::new(&device, format));

        // ── First pane (fills window minus status bar and tab bar) ─────
        let status_bar_h = crate::pane::effective_status_bar_height(!self.focus_mode);
        // At init there is always exactly one workspace, so the tab bar is
        // collapsed unless CSD is on (which reserves the title-bar strip for
        // window controls).
        let tab_bar_h = crate::pane::effective_tab_bar_height_csd(1, self.config.general.use_csd);
        let full_rect = Rect::new(
            0.0,
            tab_bar_h,
            config.width as f32,
            config.height as f32 - status_bar_h - tab_bar_h,
        );
        // tn-ou30: `scrollback`, `interceptor_cfg`, `proxy` are still used
        // by the remote attach + remote fresh-spawn paths below. The local
        // fresh-spawn path (after both remote branches fall through) is
        // deferred via `initial_pane_pending`, so it rederives them inside
        // `ensure_initial_pane_spawned`. `scan_interval_secs`,
        // `spawn_options`, and `registry` were only used by the (now
        // deferred) local path, so they have been removed here.
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let proxy = self.event_proxy.clone();

        // ── tn-5ps8: gate on `mcp.attach_mode` ─────────────────────────
        // Reads the new config field. `Local` is the default and matches
        // pre-tn-5ps8 byte-identical behaviour. `Remote` routes through
        // `RemotePty` if a daemon client is available; otherwise it
        // logs a warning and falls through to local mode so the GUI
        // still starts when the daemon is unreachable.
        let attach_mode = self.config.mcp.attach_mode;
        let use_remote = matches!(attach_mode, therminal_core::config::AttachMode::Remote)
            && self.daemon_client.is_some();
        // F8 (tn-97j6): AttachMode::Remote is now the default. Make the
        // silent default-flip discoverable in logs when the user has not
        // explicitly opted in via [mcp].attach_mode.
        if use_remote && !self.config.mcp.attach_mode_explicit {
            tracing::info!(
                "mcp.attach_mode = remote (default since tn-beez); set [mcp] attach_mode = \"local\" in therminal.toml to opt out"
            );
        }
        if matches!(attach_mode, therminal_core::config::AttachMode::Remote)
            && self.daemon_client.is_none()
        {
            tracing::warn!(
                "mcp.attach_mode = remote but no daemon client connected; falling back to local"
            );
        }

        // Use the stored handle from the leaked daemon runtime (main.rs).
        // `Handle::try_current()` is None on the winit event-loop thread
        // because that thread has no ambient tokio context.
        let remote_handle = if use_remote {
            self.daemon_runtime.clone()
        } else {
            None
        };
        if use_remote && remote_handle.is_none() {
            tracing::warn!(
                "remote attach mode requires a daemon runtime handle (set in main::connect_daemon) — falling back to local"
            );
        }
        // ── tn-ytw2: try attach to existing daemon session before create ─
        if let (true, Some(handle)) = (use_remote, remote_handle.clone()) {
            let dc = self.daemon_client.as_ref().expect("checked above").clone();
            let socket = dc.socket_path().to_path_buf();
            match self.try_attach_existing_session(
                Arc::clone(&dc),
                handle.clone(),
                socket.clone(),
                full_rect,
                &grid_renderer,
                scrollback,
                interceptor_cfg.clone(),
                proxy.clone(),
            ) {
                Ok(true) => {
                    self.window = Some(window);
                    self.gpu = Some(GpuState {
                        surface,
                        device,
                        queue,
                        config,
                    });
                    self.grid_renderer = Some(grid_renderer);
                    if let Some(wm) = self.workspaces.as_mut()
                        && let Some(renderer) = self.grid_renderer.as_ref()
                    {
                        wm.layout_mut().resize_all_panes(renderer, !self.focus_mode);
                    }
                    info!("attached to existing daemon session");
                    return;
                }
                Ok(false) => {
                    info!("no existing daemon sessions; falling through to fresh-session spawn");
                }
                Err(e) => {
                    // F10 (tn-97j6): the most common cause here is the
                    // expected ListSessions→GetWorkspaces TOCTOU race (the
                    // session was destroyed between the two RPCs). The error
                    // path is correct either way — fall through to a fresh
                    // session — but log at info! so an expected race doesn't
                    // produce a scary warn.
                    tracing::info!(error = %e, "attach to existing session failed (likely ListSessions→GetWorkspaces session-gone race); falling through to fresh-session spawn");
                }
            }
        }

        if let (true, Some(handle)) = (use_remote, remote_handle) {
            let dc = self.daemon_client.as_ref().expect("checked above").clone();
            let socket = dc.socket_path().to_path_buf();
            // tn-pgz6: pre-allocate the local pane id so the on_exit
            // callback can capture it and target the correct pane on exit.
            let local_id = crate::pane::next_pane_id();
            let p1 = proxy.clone();
            let p2 = proxy.clone();
            let p3 = proxy.clone();
            let p4 = proxy.clone();
            let on_exit_local_id = local_id;
            let on_bell_local_id = local_id;
            let callbacks = crate::pane::PaneCallbacks {
                wake: Box::new(move || {
                    let _ = p1.send_event(UserEvent::PtyOutput);
                }),
                on_exit: Box::new(move || {
                    let _ = p2.send_event(UserEvent::PaneExited(on_exit_local_id));
                }),
                on_bell: Box::new(move || {
                    let _ = p3.send_event(UserEvent::Bell(on_bell_local_id));
                }),
                on_notification: Box::new(move |text| {
                    let _ = p4.send_event(UserEvent::DesktopNotification {
                        title: "Therminal".to_string(),
                        body: text,
                        source: NotificationSource::Osc9,
                    });
                }),
            };
            // F11 (tn-97j6): if try_attach_existing_session stashed a
            // session id (empty-workspace daemon session), reuse it instead
            // of creating a fresh one and orphaning the empty one.
            let reuse_session_id = self.daemon_session_id;

            // tn-ou30: defer the remote fresh-spawn until the first
            // authoritative window size, same as the local path. The
            // stale `inner_size()` race affects remote panes equally —
            // the daemon PTY gets the wrong row count, the prompt lands
            // at the wrong position, and resize cannot retroactively
            // move it.
            self.deferred_remote_spawn = Some(super::DeferredRemoteSpawn {
                local_id,
                daemon_client: Arc::clone(&dc),
                tokio_handle: handle,
                daemon_socket: socket,
                callbacks,
                scrollback,
                interceptor_cfg: interceptor_cfg.clone(),
                reuse_session_id,
            });
            self.window = Some(window);
            self.gpu = Some(GpuState {
                surface,
                device,
                queue,
                config,
            });
            self.grid_renderer = Some(grid_renderer);
            self.initial_pane_pending = true;
            info!("remote-mode initial pane deferred until first authoritative resize (tn-ou30)");
            return;
        }

        // tn-ou30: defer the local fresh-spawn pane until the first
        // authoritative window size lands. On Windows native builds, the
        // dimensions reported by `Window::inner_size()` immediately after
        // `create_window()` can disagree with the size the OS settles on
        // before the first frame (DPI snapping, taskbar reservation, DWM
        // reshape). Spawning the shell against those stale dims causes the
        // first prompt to land at the wrong row — alacritty's resize on
        // the first real `Resized` event cannot retroactively move it,
        // and the user sees a prompt rendered mid-screen with empty
        // scrollback above and a phantom cursor at row 0.
        //
        // We commit the GPU/renderer state now (so the first redraw can
        // clear the surface) and stash the pending flag. The actual
        // spawn call happens in `ensure_initial_pane_spawned`,
        // invoked from `handle_resized` (the first authoritative size) and
        // from `handle_redraw_requested` as a fallback for platforms that
        // do not synthesize an initial `Resized` event.
        self.window = Some(window);
        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
        });
        self.grid_renderer = Some(grid_renderer);
        self.initial_pane_pending = true;
        info!("local-mode initial pane deferred until first authoritative resize (tn-ou30)");
    }

    /// tn-ou30: spawn the deferred initial pane (local or remote) against
    /// the current GPU surface dimensions.
    ///
    /// No-op unless `initial_pane_pending` is true. Called from
    /// `handle_resized` (preferred path: first authoritative size) and from
    /// `handle_redraw_requested` (fallback for platforms that do not fire
    /// an early `Resized`). Idempotent — clears the flag on success and on
    /// the first failure so we don't spin forever.
    pub(super) fn ensure_initial_pane_spawned(&mut self) {
        if !self.initial_pane_pending {
            return;
        }

        let Some(full_rect) = self.compute_layout_rect() else {
            // GPU not yet committed — keep the flag set and try again on
            // the next resize/redraw.
            return;
        };
        // Don't spawn against degenerate rects; wait for the next event.
        if full_rect.width() <= 0.0 || full_rect.height() <= 0.0 {
            return;
        }
        let Some(renderer) = self.grid_renderer.as_ref() else {
            return;
        };

        // Clear the flag eagerly so an unrecoverable failure (e.g. shell
        // spawn returning an error) doesn't loop forever on every redraw.
        self.initial_pane_pending = false;

        // ── Remote deferred spawn ────────────────────────────────────────
        if let Some(deferred) = self.deferred_remote_spawn.take() {
            let (spawn_cols, spawn_rows) = crate::pane::grid_size_for_rect(full_rect, renderer);
            info!(
                rect_w = full_rect.width(),
                rect_h = full_rect.height(),
                spawn_cols,
                spawn_rows,
                cell_w = renderer.cell_width,
                cell_h = renderer.cell_height,
                pad_x = renderer.padding_x(),
                pad_y = renderer.padding_y(),
                "deferred remote spawn: rect and grid dims"
            );
            match crate::pane::remote_spawn::spawn_remote_pane(
                deferred.local_id,
                full_rect,
                renderer,
                deferred.scrollback,
                deferred.interceptor_cfg,
                deferred.daemon_client,
                deferred.tokio_handle,
                deferred.daemon_socket,
                deferred.callbacks,
                deferred.reuse_session_id,
            ) {
                Ok((pane, daemon_session_id, daemon_pane_id)) => {
                    let pane_id = pane.id;
                    self.pane_id_map.insert(pane_id, daemon_pane_id);
                    self.daemon_session_id = Some(daemon_session_id);
                    let layout = LayoutNode::Leaf(pane);
                    let wm = WorkspaceManager::new(layout, Some(pane_id));
                    self.workspaces = Some(wm);

                    if let Some(wm) = self.workspaces.as_mut()
                        && let Some(renderer) = self.grid_renderer.as_ref()
                    {
                        let pane_count = wm.layout().pane_count();
                        let header_h =
                            crate::pane::effective_header_height(pane_count, !self.focus_mode);
                        info!(
                            pane_count,
                            header_h,
                            show_pane_headers = !self.focus_mode,
                            "resize_all_panes: effective header"
                        );
                        wm.layout_mut().resize_all_panes(renderer, !self.focus_mode);
                    }

                    // Log actual pane viewport after resize
                    if let Some(wm) = self.workspaces.as_ref()
                        && let crate::pane::LayoutNode::Leaf(pane) = wm.layout()
                        && let Some(renderer) = self.grid_renderer.as_ref()
                    {
                        let (post_cols, post_rows) =
                            crate::pane::grid_size_for_rect(pane.viewport, renderer);
                        info!(
                            pane_id,
                            post_cols,
                            post_rows,
                            viewport_w = pane.viewport.width(),
                            viewport_h = pane.viewport.height(),
                            "post-resize_all_panes: pane viewport grid dims"
                        );
                    }

                    // tn-ou30: schedule scrollback compaction after a few
                    // frames so the shell's initial output has landed.
                    self.scrollback_compact_countdown = 30;

                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "deferred remote pane spawn failed; falling back to local"
                    );
                    // Fall through to the local spawn below.
                }
            }
        }

        // ── Local deferred spawn ─────────────────────────────────────────
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
            renderer,
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
            0.0, // initial pane: no header
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "failed to spawn deferred initial pane");
                return;
            }
        };
        let pane_id = pane.id;
        let layout = LayoutNode::Leaf(pane);
        let wm = WorkspaceManager::new(layout, Some(pane_id));
        self.workspaces = Some(wm);

        // Resize initial pane with correct header height (0 for single pane).
        // This is the load-bearing call: it locks the PTY's grid dimensions
        // to the *current* surface size, not the size winit reported when
        // the window was created.
        if let Some(wm) = self.workspaces.as_mut()
            && let Some(renderer) = self.grid_renderer.as_ref()
        {
            wm.layout_mut().resize_all_panes(renderer, !self.focus_mode);
        }

        if let (Some(renderer), Some(gpu)) = (self.grid_renderer.as_ref(), self.gpu.as_ref()) {
            let (cols, rows) = renderer.grid_size(gpu.config.width, gpu.config.height);
            info!(
                pane_id,
                cols, rows, "initial local pane spawned (deferred, tn-ou30)"
            );
        }

        // tn-ou30: schedule scrollback compaction after a few frames so
        // the shell's initial output has landed.
        self.scrollback_compact_countdown = 30;

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// tn-ytw2 Phase A: try to attach to an existing daemon session and
    /// reconstruct its workspace layout into `self.workspaces`.
    ///
    /// Returns:
    /// - `Ok(true)` — attached: `self.workspaces`, `self.daemon_session_id`,
    ///   and `self.pane_id_map` are populated; caller should NOT spawn a
    ///   fresh pane.
    /// - `Ok(false)` — daemon has no sessions; caller should fall through
    ///   to fresh-session spawn.
    /// - `Err(_)` — RPC failed / timed out / returned malformed data;
    ///   caller should log and fall through to fresh-session spawn.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn try_attach_existing_session(
        &mut self,
        daemon_client: Arc<therminal_daemon_client::DaemonClient>,
        tokio_handle: tokio::runtime::Handle,
        daemon_socket: std::path::PathBuf,
        full_rect: Rect,
        renderer: &GridRenderer,
        scrollback: usize,
        interceptor_cfg: InterceptorConfig,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<bool, anyhow::Error> {
        use therminal_protocol::daemon::{IpcRequest, IpcResponse};
        let rpc_timeout = std::time::Duration::from_secs(5);

        // 1. ListSessions
        let list_resp = tokio_handle.block_on(async {
            tokio::time::timeout(
                rpc_timeout,
                daemon_client.send_request(IpcRequest::ListSessions),
            )
            .await
        });
        let list_resp = match list_resp {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!("ListSessions timed out"),
        };
        let session_ids = match list_resp {
            IpcResponse::Sessions { session_ids } => session_ids,
            IpcResponse::Error { message } => anyhow::bail!("ListSessions error: {message}"),
            other => anyhow::bail!("unexpected response to ListSessions: {other:?}"),
        };
        if session_ids.is_empty() {
            return Ok(false);
        }
        let session_id = session_ids[0];
        info!(
            session_id,
            total = session_ids.len(),
            "attaching to existing daemon session (first of N)"
        );

        // 2. GetWorkspaces
        let ws_resp = tokio_handle.block_on(async {
            tokio::time::timeout(
                rpc_timeout,
                daemon_client.send_request(IpcRequest::GetWorkspaces { session_id }),
            )
            .await
        });
        let ws_resp = match ws_resp {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e),
            Err(_) => anyhow::bail!("GetWorkspaces timed out"),
        };
        let (mut workspaces_info, active_workspace) = match ws_resp {
            IpcResponse::Workspaces {
                workspaces,
                active_workspace,
                ..
            } => (workspaces, active_workspace),
            IpcResponse::Error { message } => anyhow::bail!("GetWorkspaces error: {message}"),
            other => anyhow::bail!("unexpected response to GetWorkspaces: {other:?}"),
        };
        if workspaces_info.is_empty() {
            // F11 (tn-97j6): daemon session exists but has never published
            // workspace_state (pre-tn-k3yo persisted session). Previously we
            // returned Ok(false), which made the caller call CreateSession
            // and orphan the original empty session. Stash the session id on
            // self so the fresh-session spawn path reuses it instead.
            tracing::info!(
                session_id,
                "existing daemon session has empty workspace state; will reuse session id for fresh-pane spawn (F11)"
            );
            self.daemon_session_id = Some(session_id);
            return Ok(false);
        }
        // Stable order by `order` field.
        workspaces_info.sort_by_key(|w| w.order);

        // 3. Build leaves. We need to feed `from_workspace_info` a closure
        //    that allocates local ids, inserts into the map, and builds a
        //    remote backend per daemon pane id. We collect (local_id, daemon_id)
        //    pairs into a side buffer so we can populate `self.pane_id_map`
        //    after the WorkspaceManager has been built (the closure can't
        //    borrow self mutably while make_leaf also captures self).
        let interceptor_for_leaf = interceptor_cfg.clone();
        let dc_for_leaf = Arc::clone(&daemon_client);
        let socket_for_leaf = daemon_socket.clone();
        let handle_for_leaf = tokio_handle.clone();
        let mut id_pairs: Vec<(crate::pane::PaneId, therminal_protocol::PaneId)> = Vec::new();
        let (cols, rows) = crate::pane::grid_size_for_rect(full_rect, renderer);
        let cols = cols.max(2);
        let rows = rows.max(1);

        let wm = {
            let id_pairs = &mut id_pairs;
            let mut make_leaf =
                |daemon_pane_id: therminal_protocol::PaneId| -> Option<crate::pane::PaneState> {
                    let local_id = crate::pane::next_pane_id();
                    let p1 = proxy.clone();
                    let p2 = proxy.clone();
                    let p3 = proxy.clone();
                    let p4 = proxy.clone();
                    // Capture local_id at construction time (tn-pgz6 fix).
                    let on_exit_local_id = local_id;
                    let on_bell_local_id = local_id;
                    let callbacks = crate::pane::PaneCallbacks {
                        wake: Box::new(move || {
                            let _ = p1.send_event(UserEvent::PtyOutput);
                        }),
                        on_exit: Box::new(move || {
                            let _ = p2.send_event(UserEvent::PaneExited(on_exit_local_id));
                        }),
                        on_bell: Box::new(move || {
                            let _ = p3.send_event(UserEvent::Bell(on_bell_local_id));
                        }),
                        on_notification: Box::new(move |text| {
                            let _ = p4.send_event(UserEvent::DesktopNotification {
                                title: "Therminal".to_string(),
                                body: text,
                                source: NotificationSource::Osc9,
                            });
                        }),
                    };
                    match crate::pane::remote_spawn::build_remote_pane_state(
                        local_id,
                        daemon_pane_id,
                        full_rect,
                        cols,
                        rows,
                        scrollback,
                        interceptor_for_leaf.clone(),
                        Arc::clone(&dc_for_leaf),
                        handle_for_leaf.clone(),
                        socket_for_leaf.clone(),
                        callbacks,
                        None,
                    ) {
                        Ok(state) => {
                            id_pairs.push((local_id, daemon_pane_id));
                            Some(state)
                        }
                        Err(e) => {
                            tracing::warn!(daemon_pane_id, error = %e, "build_remote_pane_state failed during attach");
                            None
                        }
                    }
                };
            match crate::pane::WorkspaceManager::from_workspace_info(
                &workspaces_info,
                active_workspace,
                &mut make_leaf,
            ) {
                Some(wm) => wm,
                None => {
                    anyhow::bail!("WorkspaceManager::from_workspace_info returned None");
                }
            }
        };
        // 4. Populate the bidirectional pane id map.
        for (local, daemon) in &id_pairs {
            self.pane_id_map.insert(*local, *daemon);
        }
        info!(
            session_id,
            panes = id_pairs.len(),
            workspaces = workspaces_info.len(),
            "reconstructed workspace manager from daemon state"
        );

        // 5. Layout the tree against full_rect so each leaf gets a viewport,
        // then resize every Term to match its actual viewport. Without the
        // resize step the Terms are still sized to full_rect (from make_leaf)
        // so split panes have the wrong dimensions until the first window-resize
        // event fires, producing a phantom extra row and a detached cursor.
        self.workspaces = Some(wm);
        if let Some(wm) = self.workspaces.as_mut() {
            let layout = wm.layout_mut();
            layout.layout(full_rect);
            layout.resize_all_panes(renderer, !self.focus_mode);
        }
        self.daemon_session_id = Some(session_id);
        Ok(true)
    }

    /// Build PTY spawn options from the current config (shell override + env).
    pub(super) fn build_spawn_options(&self) -> therminal_terminal::pty::SpawnOptions {
        therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
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
            layout.resize_all_panes(renderer, !self.focus_mode);
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
