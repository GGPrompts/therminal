//! Settings overlay commands and configuration mutation helpers.

use therminal_core::config::{ConfigEditSession, config_path};
use tracing::{info, warn};

use super::super::settings_overlay::{self, SettingsCommand};
use super::super::{App, OverlayMode};

impl App {
    pub(super) fn open_help_overlay(&mut self) {
        self.overlay_mode = Some(OverlayMode::Help);
        self.help_overlay_scroll_rows = 0;
        self.active_menu = None;
    }

    pub(super) fn open_settings_overlay(&mut self) {
        self.overlay_mode = Some(OverlayMode::Settings);
        // Seed lazy sections (Shell, Accessibility, Hotspots, ...) before the
        // first frame. `seed_defaults` registers these sections with empty
        // control vectors, and `sync_toggle_values` → `rebuild_*_section`
        // populates them from live values. Without this call the first open
        // would render empty Shell / Accessibility until the user presses a
        // key (which is the only other sync_toggle_values site).
        let values = self.build_settings_render_values();
        self.settings_overlay.sync_toggle_values(&values);
        self.settings_overlay.reset_navigation();
        self.active_menu = None;
    }

    pub(super) fn open_launcher_overlay(&mut self) {
        self.launcher_state.build_entries(&self.config.profiles);
        // Sync grid column count with the renderer's layout.
        let sw = self
            .gpu
            .as_ref()
            .map(|g| g.config.width as f32)
            .unwrap_or(800.0);
        self.launcher_state.cols =
            super::super::launcher_overlay::compute_cols(self.launcher_state.entries.len(), sw);
        self.overlay_mode = Some(OverlayMode::Launcher);
        self.active_menu = None;
    }

    pub(super) fn close_overlay(&mut self) {
        self.overlay_mode = None;
        self.help_overlay_scroll_rows = 0;
    }

    /// Show the trust escalation modal overlay (tn-b99).
    #[allow(dead_code)]
    pub(crate) fn show_trust_escalation(
        &mut self,
        escalation_id: u64,
        agent_name: String,
        tool_name: String,
        current_tier: String,
        required_tier: String,
    ) {
        self.trust_escalation = Some(
            super::super::trust_escalation_overlay::TrustEscalationState {
                escalation_id,
                agent_name,
                tool_name,
                current_tier,
                required_tier,
                approve_focused: true,
            },
        );
        self.overlay_mode = Some(OverlayMode::TrustEscalation);
        self.active_menu = None;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Resolve the pending trust escalation and send the response to the daemon.
    pub(super) fn resolve_trust_escalation(&mut self, approved: bool) {
        if let Some(state) = self.trust_escalation.take() {
            info!(
                escalation_id = state.escalation_id,
                approved,
                agent = %state.agent_name,
                tool = %state.tool_name,
                "trust escalation resolved"
            );
            // Send IPC response to daemon via the daemon client.
            if let (Some(client), Some(handle)) = (&self.daemon_client, &self.daemon_runtime) {
                let esc_id = state.escalation_id;
                let client = client.clone();
                let handle = handle.clone();
                handle.spawn(async move {
                    use therminal_protocol::daemon::IpcRequest;
                    let req = IpcRequest::ResolveTrustEscalation {
                        escalation_id: esc_id,
                        approved,
                    };
                    if let Err(e) = client.send_request(req).await {
                        tracing::warn!(%e, "failed to send trust escalation response");
                    }
                });
            }
        }
        self.close_overlay();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Spawn a new pane with the selected launcher profile.
    pub(super) fn launch_selected_profile(&mut self) {
        let entry = match self.launcher_state.selected_entry() {
            Some(e) => e.clone(),
            None => {
                self.close_overlay();
                return;
            }
        };
        self.close_overlay();

        let profile_name = entry.profile_name;

        // tn-khmo: URL-bearing tiles spawn a WebView pane instead of a
        // shell pane. We follow the tn-ojy9 / tn-t0gp precedent and call
        // `create_webview_pane()` directly on the main thread — no daemon
        // round-trip needed for GUI-originated spawns. (The
        // `IpcRequest::SpawnWebViewPane` route remains the stable contract
        // for MCP/CLI callers and still bounces back through
        // `SpawnWebViewPaneRequested`.)
        if let Some(url) = entry.url {
            info!(
                profile = ?profile_name,
                url = %url,
                "launcher: spawning webview pane"
            );
            if let Err(e) = self.create_webview_pane(&url) {
                warn!(url = %url, error = %e, "launcher: webview spawn failed");
                self.show_toast(format!("WebView spawn failed: {e}"));
            }
            return;
        }

        info!(
            profile = ?profile_name,
            "launcher: spawning pane"
        );

        if self.is_daemon_mode() {
            self.launch_profile_remote(profile_name);
        } else {
            // Local mode: use default split (no profile support in local mode).
            self.split_focused_pane(crate::pane::SplitDirection::Horizontal);
        }
    }

    /// Fire a daemon `SplitPane` RPC with the selected profile name.
    fn launch_profile_remote(&mut self, profile_name: Option<String>) {
        use std::sync::Arc;
        use therminal_protocol::daemon::{IpcRequest, IpcResponse};

        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let daemon_source = match self.pane_id_map.daemon_for_local(focused) {
            Some(d) => d,
            None => {
                warn!("launcher: no daemon id for focused pane");
                return;
            }
        };

        let inherited_cwd = self
            .get_layout()
            .and_then(|layout| super::super::pane_ops::cwd_from_source_pane(layout, focused));

        let Some(client) = self.daemon_client.as_ref() else {
            return;
        };
        let Some(handle) = self.daemon_runtime.as_ref() else {
            return;
        };
        let client = Arc::clone(client);
        let proxy = self.event_proxy.clone();
        let cwd_clone = inherited_cwd.clone();

        handle.spawn(async move {
            let rpc_result = match tokio::time::timeout(
                super::super::pane_ops::DAEMON_OP_TIMEOUT,
                client.send_request(IpcRequest::SplitPane {
                    pane_id: daemon_source,
                    horizontal: true,
                    cwd: cwd_clone,
                    startup_command: None,
                    ratio: None,
                    shell: None,
                    worktree: None,
                    profile: profile_name,
                }),
            )
            .await
            {
                Ok(Ok(IpcResponse::PaneSplit { new_pane_id })) => Ok(new_pane_id),
                Ok(Ok(IpcResponse::Error { message })) => Err(message),
                Ok(Ok(other)) => Err(format!("unexpected response: {other:?}")),
                Ok(Err(e)) => Err(format!("rpc error: {e}")),
                Err(_) => Err("rpc timed out".to_string()),
            };
            let _ = proxy.send_event(super::super::UserEvent::DaemonSplitComplete(
                crate::window::pane_ops::DaemonSplitResult {
                    source_local: focused,
                    direction: crate::pane::SplitDirection::Horizontal,
                    rpc_result,
                    inherited_cwd: None,
                    on_complete: crate::window::pane_ops::DaemonSplitOnComplete::FocusAndRelayout,
                },
            ));
        });
    }

    pub(super) fn build_settings_render_values(&self) -> settings_overlay::SettingsRenderValues {
        use therminal_core::config::NewPaneCwd;
        settings_overlay::SettingsRenderValues {
            editor_chain: self.config.hotspots.editor_chain.clone(),
            folder_pane_command: self.config.hotspots.folder_pane_command.clone(),
            folder_opener: self.config.hotspots.folder_opener.clone(),
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.join(" "),
            new_pane_cwd_index: match self.config.general.new_pane_cwd {
                NewPaneCwd::Inherit => 0,
                NewPaneCwd::Home => 1,
            },
            high_contrast: self.config.accessibility.high_contrast,
            reduced_motion: self.config.accessibility.reduced_motion,
            ui_text_scale_index: settings_overlay::ui_text_scale_index(
                self.config.accessibility.ui_text_scale,
            ),
            font_family_index: settings_overlay::font_family_index(&self.config.font.family),
            available_font_families: self
                .grid_renderer
                .as_ref()
                .map(|r| {
                    settings_overlay::FONT_FAMILY_OPTIONS
                        .iter()
                        .filter(|name| r.is_font_available(name))
                        .map(|s| (*s).to_string())
                        .collect()
                })
                .unwrap_or_default(),
            // Cursor (tn-ya01)
            cursor_style_index: settings_overlay::cursor_style_index(&self.config.cursor.style),
            cursor_blink: self.config.cursor.blink,
            // Notifications (tn-ya01)
            bell_style_index: settings_overlay::bell_style_index(&self.config.bell.style),
            agent_waiting: self.config.notifications.agent_waiting,
            osc9_enabled: self.config.notifications.osc9_enabled,
            // Terminal (tn-ya01)
            auto_tile: self.config.general.auto_tile,
            scrollback_index: settings_overlay::scrollback_index(
                self.config.general.scrollback_lines,
            ),
            // Widgets (tn-ya01)
            system_metrics_enabled: self.config.widgets.system_metrics.enabled,
            // Appearance (tn-ldjq)
            background_opacity_index: settings_overlay::background_opacity_index(
                self.config.colors.background_opacity(),
            ),
        }
    }

    pub(super) fn persist_settings_overlay_edits(&mut self) {
        let mut edit = ConfigEditSession::from_saved(self.config.clone());
        let path = config_path();
        if let Err(e) = edit.save_draft_to(&path) {
            warn!(?path, %e, "failed to persist settings overlay edits");
            self.show_toast("settings save failed (see logs)");
            return;
        }
        info!(?path, "persisted settings overlay edits");
    }

    pub(super) fn apply_settings_command(&mut self, command: SettingsCommand) {
        match command {
            SettingsCommand::ApplyThemePreset(preset) => {
                settings_overlay::apply_theme_preset(&mut self.config.colors, preset);
                if let Some(renderer) = self.grid_renderer.as_mut() {
                    renderer.apply_color_overrides_with_contrast(
                        &self.config.colors,
                        self.config.accessibility.high_contrast,
                    );
                }
                self.settings_overlay.set_active_theme(preset);
                self.show_toast(format!("theme preset applied: {}", preset.label()));
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // -- Hotspot mutations (tn-avjv.5) --
            SettingsCommand::EditorChainRemove(idx) => {
                if idx < self.config.hotspots.editor_chain.len() {
                    let removed = self.config.hotspots.editor_chain.remove(idx);
                    self.show_toast(format!("removed editor: {removed}"));
                }
            }
            SettingsCommand::EditorChainEdit(idx, value) => {
                if value.is_empty() {
                    if idx < self.config.hotspots.editor_chain.len() {
                        let removed = self.config.hotspots.editor_chain.remove(idx);
                        self.show_toast(format!("removed editor: {removed}"));
                    }
                } else if idx < self.config.hotspots.editor_chain.len() {
                    self.config.hotspots.editor_chain[idx] = value;
                    self.show_toast("editor updated");
                }
            }
            SettingsCommand::EditorChainAdd(value) => {
                let entry = if value.is_empty() {
                    "nano".to_string()
                } else {
                    value
                };
                self.config.hotspots.editor_chain.push(entry.clone());
                self.show_toast(format!("added editor: {entry}"));
            }
            SettingsCommand::EditorChainMoveUp(idx) => {
                if idx > 0 && idx < self.config.hotspots.editor_chain.len() {
                    self.config.hotspots.editor_chain.swap(idx, idx - 1);
                }
            }
            SettingsCommand::EditorChainMoveDown(idx) => {
                if idx + 1 < self.config.hotspots.editor_chain.len() {
                    self.config.hotspots.editor_chain.swap(idx, idx + 1);
                }
            }
            SettingsCommand::SetFolderPaneCommand(text) => {
                let parts: Vec<String> = text.split_whitespace().map(String::from).collect();
                self.config.hotspots.folder_pane_command = parts;
                self.show_toast("folder pane command updated");
            }
            SettingsCommand::FolderOpenerRemove(idx) => {
                if idx < self.config.hotspots.folder_opener.len() {
                    let removed = self.config.hotspots.folder_opener.remove(idx);
                    self.show_toast(format!("removed opener: {removed}"));
                }
            }
            SettingsCommand::FolderOpenerEdit(idx, value) => {
                if value.is_empty() {
                    if idx < self.config.hotspots.folder_opener.len() {
                        let removed = self.config.hotspots.folder_opener.remove(idx);
                        self.show_toast(format!("removed opener: {removed}"));
                    }
                } else if idx < self.config.hotspots.folder_opener.len() {
                    self.config.hotspots.folder_opener[idx] = value;
                    self.show_toast("opener updated");
                }
            }
            SettingsCommand::FolderOpenerAdd(value) => {
                let entry = if value.is_empty() {
                    "xdg-open".to_string()
                } else {
                    value
                };
                self.config.hotspots.folder_opener.push(entry.clone());
                self.show_toast(format!("added opener: {entry}"));
            }
            SettingsCommand::FolderOpenerMoveUp(idx) => {
                if idx > 0 && idx < self.config.hotspots.folder_opener.len() {
                    self.config.hotspots.folder_opener.swap(idx, idx - 1);
                }
            }
            SettingsCommand::FolderOpenerMoveDown(idx) => {
                if idx + 1 < self.config.hotspots.folder_opener.len() {
                    self.config.hotspots.folder_opener.swap(idx, idx + 1);
                }
            }
            // -- Shell mutations (tn-avjv.6) --
            SettingsCommand::SetDefaultShell(shell) => {
                self.config.general.shell = shell;
                self.show_toast("default shell updated");
            }
            SettingsCommand::SetShellArgs(args_str) => {
                let args: Vec<String> = args_str.split_whitespace().map(String::from).collect();
                self.config.general.shell_args = args;
                self.show_toast("shell args updated");
            }
            SettingsCommand::SetNewPaneCwd(idx) => {
                use therminal_core::config::NewPaneCwd;
                self.config.general.new_pane_cwd = match idx {
                    0 => NewPaneCwd::Inherit,
                    _ => NewPaneCwd::Home,
                };
                self.show_toast(format!(
                    "new pane cwd: {:?}",
                    self.config.general.new_pane_cwd
                ));
            }
            // -- Accessibility mutations (tn-avjv.6) --
            SettingsCommand::ToggleHighContrast => {
                self.config.accessibility.high_contrast = !self.config.accessibility.high_contrast;
                // Rebuild chrome palette with the new contrast setting.
                if let Some(renderer) = self.grid_renderer.as_mut() {
                    renderer.apply_color_overrides_with_contrast(
                        &self.config.colors,
                        self.config.accessibility.high_contrast,
                    );
                }
                let label = if self.config.accessibility.high_contrast {
                    "on"
                } else {
                    "off"
                };
                self.show_toast(format!("high contrast chrome: {label}"));
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            SettingsCommand::ToggleReducedMotion => {
                self.config.accessibility.reduced_motion =
                    !self.config.accessibility.reduced_motion;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            SettingsCommand::SetUiTextScale(idx) => {
                let scale = settings_overlay::UI_TEXT_SCALE_OPTIONS
                    .get(idx)
                    .copied()
                    .unwrap_or(1.0);
                self.config.accessibility.ui_text_scale = scale;
                if let Some(renderer) = self.grid_renderer.as_mut() {
                    renderer.set_ui_text_scale(scale);
                }
                self.show_toast(format!("UI text scale: {:.0}%", scale * 100.0));
                self.relayout_and_redraw();
            }
            // -- Cursor mutations (tn-ya01) --
            SettingsCommand::SetCursorStyle(idx) => {
                use therminal_core::config::CursorStyle;
                self.config.cursor.style = match idx {
                    0 => CursorStyle::Block,
                    1 => CursorStyle::Underline,
                    2 => CursorStyle::Beam,
                    3 => CursorStyle::HollowBlock,
                    _ => CursorStyle::Block,
                };
                self.show_toast(format!("cursor style: {:?}", self.config.cursor.style));
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            SettingsCommand::ToggleCursorBlink => {
                self.config.cursor.blink = !self.config.cursor.blink;
                // Reset blink state so cursor is immediately visible after toggle.
                self.cursor_blink_visible = true;
                self.last_cursor_blink = std::time::Instant::now();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // -- Notification mutations (tn-ya01) --
            SettingsCommand::SetBellStyle(idx) => {
                use therminal_core::config::BellStyle;
                self.config.bell.style = match idx {
                    0 => BellStyle::Taskbar,
                    1 => BellStyle::Visual,
                    2 => BellStyle::None,
                    _ => BellStyle::Taskbar,
                };
                self.show_toast(format!("bell style: {:?}", self.config.bell.style));
            }
            SettingsCommand::ToggleAgentWaiting => {
                self.config.notifications.agent_waiting = !self.config.notifications.agent_waiting;
                let label = if self.config.notifications.agent_waiting {
                    "on"
                } else {
                    "off"
                };
                self.show_toast(format!("agent waiting notifications: {label}"));
            }
            SettingsCommand::ToggleOsc9Enabled => {
                self.config.notifications.osc9_enabled = !self.config.notifications.osc9_enabled;
                let label = if self.config.notifications.osc9_enabled {
                    "on"
                } else {
                    "off"
                };
                self.show_toast(format!("desktop notifications (OSC 9): {label}"));
            }
            // -- Terminal mutations (tn-ya01) --
            SettingsCommand::ToggleAutoTile => {
                self.config.general.auto_tile = !self.config.general.auto_tile;
                let label = if self.config.general.auto_tile {
                    "on"
                } else {
                    "off"
                };
                self.show_toast(format!("auto-tile: {label}"));
            }
            SettingsCommand::SetScrollbackLines(idx) => {
                let lines = settings_overlay::SCROLLBACK_OPTIONS
                    .get(idx)
                    .copied()
                    .unwrap_or(10_000);
                self.config.general.scrollback_lines = lines;
                self.show_toast(format!("scrollback: {lines} lines"));
            }
            // -- Widget mutations (tn-ya01) --
            SettingsCommand::ToggleSystemMetrics => {
                self.config.widgets.system_metrics.enabled =
                    !self.config.widgets.system_metrics.enabled;
                let label = if self.config.widgets.system_metrics.enabled {
                    "on"
                } else {
                    "off"
                };
                self.show_toast(format!("system metrics: {label}"));
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // -- Appearance mutations (tn-ldjq) --
            SettingsCommand::SetBackgroundOpacity(idx) => {
                let opacity = settings_overlay::BACKGROUND_OPACITY_OPTIONS
                    .get(idx)
                    .copied()
                    .unwrap_or(1.0);
                self.config.colors.background_opacity = Some(opacity);
                // Rebuild chrome palette so bg-derived chrome alpha updates.
                if let Some(renderer) = self.grid_renderer.as_mut() {
                    renderer.apply_color_overrides_with_contrast(
                        &self.config.colors,
                        self.config.accessibility.high_contrast,
                    );
                }
                self.show_toast(format!("background opacity: {:.0}%", opacity * 100.0));
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // -- Font mutations (tn-0zfo) --
            SettingsCommand::SetFontFamily(ref family) => {
                if family.is_empty() {
                    return;
                }
                self.config.font.family = family.clone();
                self.config.font.ui_font_family = family.clone();
                // Trigger font reload: build a new grid_renderer FontConfig
                // and call update_font, mirroring the apply_config hot-reload
                // path in window/mod.rs.
                if let (Some(renderer), Some(gpu), Some(window)) = (
                    self.grid_renderer.as_mut(),
                    self.gpu.as_ref(),
                    self.window.as_ref(),
                ) {
                    let scale = window.scale_factor() as f32;
                    let mut new_font_config = crate::grid_renderer::FontConfig::new(
                        family.clone(),
                        self.config.font.size * scale,
                    );
                    new_font_config.fallback_families = self.config.font.extra_fallbacks.clone();
                    new_font_config.ui_font_family = family.clone();
                    new_font_config.line_height =
                        self.config.font.size * self.config.font.line_height_scale * scale;
                    renderer.update_font(
                        new_font_config,
                        &gpu.device,
                        &gpu.queue,
                        gpu.config.width,
                        gpu.config.height,
                    );
                }
                self.show_toast(format!("font family: {family}"));
                self.relayout_and_redraw();
            }
        }
        self.persist_settings_overlay_edits();
    }
}
