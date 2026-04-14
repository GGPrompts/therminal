//! Window event dispatch helpers.
//!
//! `App::window_event` in `mod.rs` is now a thin match that delegates each
//! variant to a `pub(super) handle_*` method on this `impl App` block.

use std::time::{Duration, Instant};

use tracing::{info, warn};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};

use therminal_core::config::{ConfigEditSession, KeyAction, config_path};
use therminal_terminal::input::{self, KeyCode, Modifiers as InputModifiers};

use super::keybindings::lookup_binding;
use super::mouse::HeaderAction;
use super::render_driver::JumpDirection;
use super::settings_overlay::{self, SettingsCommand};
use super::{App, OverlayMode, chrome, help_overlay};
use crate::pane::SplitDirection;

impl App {
    fn help_overlay_max_scroll_rows(&self) -> u32 {
        let Some(gpu) = self.gpu.as_ref() else {
            return 0;
        };
        help_overlay::max_scroll_rows(
            &self.config.keybindings,
            gpu.config.width,
            gpu.config.height,
        )
    }

    fn scroll_help_overlay_by(&mut self, delta_rows: i32) {
        let max_rows = self.help_overlay_max_scroll_rows();
        let next = (self.help_overlay_scroll_rows as i32 + delta_rows).clamp(0, max_rows as i32);
        self.help_overlay_scroll_rows = next as u32;
    }

    fn open_help_overlay(&mut self) {
        self.overlay_mode = Some(OverlayMode::Help);
        self.help_overlay_scroll_rows = 0;
        self.active_menu = None;
    }

    fn open_settings_overlay(&mut self) {
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

    fn close_overlay(&mut self) {
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
        self.trust_escalation = Some(super::trust_escalation_overlay::TrustEscalationState {
            escalation_id,
            agent_name,
            tool_name,
            current_tier,
            required_tier,
            approve_focused: true,
        });
        self.overlay_mode = Some(OverlayMode::TrustEscalation);
        self.active_menu = None;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Resolve the pending trust escalation and send the response to the daemon.
    fn resolve_trust_escalation(&mut self, approved: bool) {
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
        }
    }

    fn persist_settings_overlay_edits(&mut self) {
        let mut edit = ConfigEditSession::from_saved(self.config.clone());
        let path = config_path();
        if let Err(e) = edit.save_draft_to(&path) {
            warn!(?path, %e, "failed to persist settings overlay edits");
            self.show_toast("settings save failed (see logs)");
            return;
        }
        info!(?path, "persisted settings overlay edits");
    }

    fn apply_settings_command(&mut self, command: SettingsCommand) {
        match command {
            SettingsCommand::ApplyThemePreset(preset) => {
                settings_overlay::apply_theme_preset(&mut self.config.colors, preset);
                if let Some(renderer) = self.grid_renderer.as_mut() {
                    renderer.apply_color_overrides(&self.config.colors);
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
        }
        self.persist_settings_overlay_edits();
    }

    /// Check if this key event matches a configured keybinding.
    /// Returns true if the event was consumed.
    pub(super) fn handle_keybinding(&mut self, key_event: &KeyEvent) -> bool {
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
            KeyAction::ResizeReset => self.reset_all_ratios(),
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
                self.zoom_toggle_focused_pane();
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
                if self.overlay_mode == Some(OverlayMode::Help) {
                    self.close_overlay();
                } else {
                    self.open_help_overlay();
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            KeyAction::ShowSettings => {
                if self.overlay_mode == Some(OverlayMode::Settings) {
                    self.close_overlay();
                } else {
                    self.open_settings_overlay();
                }
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
            KeyAction::NewWorkspace => {
                self.create_new_workspace();
            }
            KeyAction::RenameWorkspace => {
                if let Some(ws_id) = self.workspaces.as_ref().map(|wm| wm.active_id()) {
                    self.start_rename_workspace(ws_id);
                }
            }
            KeyAction::JumpRegionPrev => {
                self.jump_to_region(JumpDirection::Prev, false);
            }
            KeyAction::JumpRegionNext => {
                self.jump_to_region(JumpDirection::Next, false);
            }
            KeyAction::JumpErrorPrev => {
                self.jump_to_region(JumpDirection::Prev, true);
            }
            KeyAction::JumpErrorNext => {
                self.jump_to_region(JumpDirection::Next, true);
            }
            KeyAction::ToggleAgentTimeline => {
                self.agent_timeline.toggle();
                if !self.agent_timeline.visible {
                    // Remove the cached texture so it doesn't linger.
                    self.widget_manager
                        .remove(crate::widgets::TIMELINE_WIDGET_ID);
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            KeyAction::FocusMode => {
                // tn-t2yd.2: toggle runtime focus mode (hide/show all chrome).
                self.focus_mode = !self.focus_mode;
                // tn-sfn9: clear hover hint when leaving focus mode.
                if !self.focus_mode {
                    self.focus_mode_hint_visible = false;
                }
                info!(focus_mode = self.focus_mode, "focus mode toggled");
                self.relayout_and_redraw();
            }
            // Hotspot actions are menu-only; they shouldn't reach keybinding dispatch.
            KeyAction::HotspotCopy(_)
            | KeyAction::HotspotOpenInEditor(_)
            | KeyAction::HotspotOpenExternal(_)
            | KeyAction::HotspotOpenFolderInPane(_)
            | KeyAction::HotspotOpenFolderInFileManager(_)
            | KeyAction::HotspotShowGitRef { .. } => {}
        }
        true
    }

    /// Handle a keyboard event: encode it and write to the focused pane's PTY.
    pub(super) fn handle_key_input(&mut self, key_event: &KeyEvent) {
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
            && let Err(e) = pane.write_input(&bytes)
        {
            warn!("Failed to write to pane {} PTY: {e}", pane.id);
        }
    }

    /// Open a context menu at the given pixel position, or pass through to
    /// the PTY if the pane has mouse reporting enabled (like tmux does).
    pub(super) fn open_context_menu(&mut self, px: f32, py: f32) {
        let pane_id = match self.pane_at_position(px as f64, py as f64) {
            Some(id) => id,
            None => return,
        };

        // Smart pass-through: if the terminal app has mouse reporting enabled,
        // forward the right-click to the PTY instead of showing our menu.
        // This lets TUI apps like btop, lazygit, etc. handle their own menus.
        let mode = self.pane_term_mode(pane_id);
        let mouse_mode = mode.contains(alacritty_terminal::term::TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(alacritty_terminal::term::TermMode::SGR_MOUSE)
            || mode.contains(alacritty_terminal::term::TermMode::MOUSE_DRAG)
            || mode.contains(alacritty_terminal::term::TermMode::MOUSE_MOTION);

        if mouse_mode {
            // Encode the right-click as a mouse press event and send to the PTY.
            if let Some((col, row)) = self.pixel_to_grid_for_pane(px as f64, py as f64, pane_id) {
                let mods = self.input_mods();
                let bytes = therminal_terminal::input::encode_mouse_press(
                    therminal_terminal::input::MouseButton::Right,
                    col,
                    row,
                    &mods,
                );
                self.pty_write_to_pane(&bytes, pane_id);
            }
            return;
        }

        let bindings = &self.config.keybindings.bindings;

        // Check if the pane under the cursor has a selection.
        let has_selection = if let Some(layout) = self.get_layout() {
            if let Some(pane) = layout.find_pane(pane_id) {
                pane.backend
                    .term()
                    .and_then(|t| t.lock().selection_to_string())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            } else {
                false
            }
        } else {
            false
        };

        let mut menu = if has_selection {
            let text = self
                .get_layout()
                .and_then(|l| l.find_pane(pane_id))
                .and_then(|p| p.backend.term())
                .and_then(|t| t.lock().selection_to_string())
                .unwrap_or_default();
            crate::menu::build_selection_menu(text, bindings, (px, py))
        } else {
            crate::menu::build_pane_menu(pane_id, bindings, (px, py))
        };

        // If a hotspot sits under the cursor, prepend hotspot-specific
        // action sections so both the hotspot actions and the generic pane
        // actions are available in a single merged menu.
        if !has_selection
            && let Some((col, row)) = self.pixel_to_grid_for_pane(px as f64, py as f64, pane_id)
            && let Some((kind, text, is_dir, resolved)) = self
                .grid_renderer
                .as_ref()
                .and_then(|r| r.hotspot_map.get(&(pane_id, row, col)).cloned())
        {
            // Prefer the harness-resolved absolute path when present
            // (tn-gidy) so right-click menu actions also survive agent
            // worktree hops.
            let effective = resolved.unwrap_or(text);
            let hotspot_menu = crate::menu::build_hotspot_palette(
                kind,
                effective.to_string(),
                is_dir,
                &self.discovered_git_tools,
                (px, py),
            );
            let mut merged = hotspot_menu.sections;
            merged.append(&mut menu.sections);
            menu.sections = merged;
        }

        self.active_menu = Some(menu);
    }

    /// Execute the currently selected menu action and close the menu.
    pub(super) fn execute_menu_action(&mut self) {
        let (action, menu_pane_id) = match self.active_menu.as_ref() {
            Some(m) => {
                let action = match m.selected_action() {
                    Some(a) => a,
                    None => {
                        self.active_menu = None;
                        return;
                    }
                };
                let pane_id = match m.context {
                    crate::menu::MenuContext::Pane { pane_id } => Some(pane_id),
                    _ => None,
                };
                (action, pane_id)
            }
            None => {
                return;
            }
        };
        self.active_menu = None;

        match action {
            KeyAction::SplitHorizontal => self.split_focused_pane(SplitDirection::Horizontal),
            KeyAction::SplitVertical => self.split_focused_pane(SplitDirection::Vertical),
            KeyAction::SplitAuto => self.split_focused_pane_auto(),
            KeyAction::ClosePane => {
                if let Some(id) = menu_pane_id {
                    self.close_pane_by_id(id);
                } else {
                    self.close_focused_pane();
                }
            }
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
            KeyAction::HotspotOpenFolderInPane(ref path) => {
                self.open_folder_in_pane(path);
            }
            KeyAction::HotspotOpenFolderInFileManager(ref path) => {
                self.open_folder_in_file_manager(path);
            }
            KeyAction::HotspotShowGitRef { ref tool, ref hash } => {
                self.show_git_ref_in_pane(tool, hash);
            }
            KeyAction::NewWorkspace => self.create_new_workspace(),
            KeyAction::RenameWorkspace => {
                let ws_id = self
                    .tab_menu_workspace_id
                    .take()
                    .or_else(|| self.workspaces.as_ref().map(|wm| wm.active_id()));
                if let Some(ws_id) = ws_id {
                    self.start_rename_workspace(ws_id);
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

    // ── window_event sub-handlers ───────────────────────────────────────

    pub(super) fn handle_resized(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        info!(
            width = new_size.width,
            height = new_size.height,
            pane_pending = self.initial_pane_pending,
            has_panes = self.workspaces.is_some(),
            minimized = self.minimized,
            "handle_resized"
        );
        if new_size.width == 0 || new_size.height == 0 {
            // tn-rl6i: on Windows, minimizing sends Resized(0, 0).
            self.minimized = true;
            return;
        }

        // tn-rl6i: restoring from minimized state — force a full relayout
        // so the grid renders at the correct dimensions. Without this, the
        // terminal shows garbled single-syllable text wrapping at wrong
        // column widths until the user manually resizes.
        let was_minimized = self.minimized;
        self.minimized = false;

        let now = Instant::now();
        let elapsed_ok = self
            .last_resize_at
            .map(|t| now.duration_since(t).as_millis() > 16)
            .unwrap_or(true);
        if elapsed_ok || was_minimized {
            self.last_resize_at = Some(now);
            self.pending_resize = None;
            self.resize(new_size);
            if was_minimized {
                self.relayout_and_redraw();
            } else {
                // tn-ou30: do NOT spawn the deferred pane from handle_resized.
                // On Windows the OS can fire multiple resize events in rapid
                // succession before settling on the final size (e.g. maximized
                // → actual default). Spawning on the first resize would create
                // the PTY at the wrong dimensions, then the subsequent shrink
                // pushes content into scrollback. The spawn happens instead in
                // handle_redraw_requested, which runs after the event batch is
                // drained and gpu.config reflects the settled size.
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
        } else {
            self.pending_resize = Some(new_size);
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    pub(super) fn handle_scale_factor_changed(&mut self) {
        let new_size = self.window.as_ref().map(|w| w.inner_size());
        if let Some(size) = new_size {
            self.resize(size);
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    pub(super) fn handle_redraw_requested(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(size) = self.pending_resize.take() {
            self.last_resize_at = Some(Instant::now());
            self.resize(size);
        }

        // tn-ou30: fallback for platforms that do not synthesize an early
        // `WindowEvent::Resized` after window creation. By the time we
        // enter the first redraw, `gpu.config.{width,height}` reflect the
        // real surface dimensions (set in `init_gpu` from
        // `window.inner_size()`), so spawning here is safe even if no
        // resize event has fired yet. On the typical Windows path the
        // first `Resized` already covered this; this call is then a
        // no-op (the flag was cleared there).
        self.ensure_initial_pane_spawned();

        // workspaces == None means all panes are gone and no restore
        // is pending — exit the window. Gate on `initial_pane_pending`
        // (tn-ou30) so we don't exit during the brief window between
        // `init_gpu` returning and the deferred initial spawn landing.
        if self.workspaces.is_none() && !self.initial_pane_pending {
            event_loop.exit();
            return;
        }

        // tn-ou30: scrollback compaction after initial pane spawn.
        // The shell may emit a leading newline or startup text that
        // creates spurious scrollback before the first prompt. A
        // resize-down-then-up cycle compacts it away — same technique
        // as xterm.js "resize trick".
        if self.scrollback_compact_countdown > 0 {
            self.scrollback_compact_countdown -= 1;
            if self.scrollback_compact_countdown == 0
                && let Some(wm) = self.workspaces.as_mut()
            {
                wm.layout_mut().compact_scrollback();
            }
            // Keep requesting redraws until the countdown expires.
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }

        // Poll auto-tile debouncer for agent spawn/exit actions.
        self.poll_auto_tile();
        // Poll swarm-watcher debouncer for subagent spawn/reclaim actions.
        self.poll_swarm_watcher();

        self.render();

        // If either debouncer has pending events, schedule another redraw
        // so we can check again after the debounce interval expires.
        let auto_pending = self
            .auto_tile_debouncer
            .as_ref()
            .is_some_and(|d| d.has_pending());
        let swarm_pending = self
            .swarm_debouncer
            .as_ref()
            .is_some_and(|d| d.has_pending());
        if (auto_pending || swarm_pending)
            && let Some(w) = self.window.as_ref()
        {
            w.request_redraw();
        }
    }

    pub(super) fn handle_cursor_moved_event(&mut self, position: PhysicalPosition<f64>) {
        // Update cursor position for menu hover tracking.
        self.cursor_position = Some((position.x, position.y));
        if let Some(menu) = self.active_menu.as_mut() {
            if let Some(gpu) = self.gpu.as_ref() {
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

    pub(super) fn handle_mouse_wheel_event(&mut self, delta: MouseScrollDelta) {
        // Ignore scroll when context menu is open.
        if self.active_menu.is_some() {
            return;
        }
        match self.overlay_mode {
            Some(OverlayMode::Help) => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 24.0).round() as i32,
                };
                // Wheel up (positive y) scrolls toward the top.
                self.scroll_help_overlay_by(-lines);
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return;
            }
            Some(OverlayMode::Settings) => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 24.0).round() as i32,
                };
                // Scroll through controls: wheel up moves selection up.
                for _ in 0..lines.unsigned_abs() {
                    if lines < 0 {
                        self.settings_overlay.arrow_down();
                    } else {
                        self.settings_overlay.arrow_up();
                    }
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return;
            }
            Some(OverlayMode::TrustEscalation) => {
                // No scrolling for trust escalation modal.
                return;
            }
            None => {}
        }
        self.handle_mouse_wheel(delta);
    }

    pub(super) fn handle_mouse_input_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        state: ElementState,
        button: MouseButton,
    ) {
        // Any mouse press while inline rename is active commits the rename
        // (click-outside semantics). The click then proceeds normally.
        if self.rename_state.is_some() && state == ElementState::Pressed {
            self.commit_rename();
        }

        // Overlay mouse interaction: click inside the settings panel is
        // consumed (don't pass to terminal), click outside closes overlay.
        // Help overlay still uses dismiss-on-any-click.
        if self.overlay_mode.is_some() && state == ElementState::Pressed {
            if let Some(OverlayMode::Settings) = self.overlay_mode {
                let (px, py) = self.cursor_position.unwrap_or((0.0, 0.0));
                if self.settings_overlay.contains_point(px as f32, py as f32) {
                    // Click inside the panel — consume without closing.
                    return;
                }
            }
            self.close_overlay();
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
            return;
        }

        // ── Context menu interception ──────────────────────────────
        if self.active_menu.is_some() && state == ElementState::Pressed {
            if button == MouseButton::Left {
                if let (Some((px, py)), Some(menu), Some(gpu)) = (
                    self.cursor_position,
                    self.active_menu.as_ref(),
                    self.gpu.as_ref(),
                ) {
                    let geo = menu.geometry(gpu.config.width as f32, gpu.config.height as f32);
                    let inside = menu.contains_point(px as f32, py as f32, geo.width, geo.height);
                    let idx = if inside {
                        menu.item_at_position(
                            px as f32,
                            py as f32,
                            geo.x,
                            geo.y,
                            geo.width,
                            geo.item_height,
                            geo.section_gap,
                        )
                    } else {
                        None
                    };
                    if inside {
                        if let Some(idx) = idx {
                            if let Some(m) = self.active_menu.as_mut() {
                                m.selected_index = Some(idx);
                            }
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

        // ── Right-click: tab bar menu or pane context menu ──────────
        if state == ElementState::Pressed
            && button == MouseButton::Right
            && let Some((px, py)) = self.cursor_position
        {
            // Check if the right-click is in the tab bar area.
            // tn-t2yd.4: CSD mode keeps the strip reserved even for a single
            // workspace, and we now render the tab there too, so right-click
            // should open the tab menu in that case as well.
            let workspace_count = self.workspaces.as_ref().map(|wm| wm.len()).unwrap_or(1);
            let tab_bar_visible = crate::pane::should_show_tab_bar(workspace_count);
            let use_csd = self.config.general.use_csd;
            let tab_bar_h = crate::pane::effective_tab_bar_height_csd(
                workspace_count,
                use_csd,
                self.focus_mode,
            );
            // tn-sfn9: focus mode hides the tab bar entirely, skip hit-testing.
            if !self.focus_mode && (tab_bar_visible || use_csd) && (py as f32) < tab_bar_h {
                // Right-click on a tab: open tab context menu.
                let workspace_ids = self
                    .workspaces
                    .as_ref()
                    .map(|wm| wm.workspace_ids())
                    .unwrap_or_default();
                let tab_labels = super::build_tab_labels(
                    &workspace_ids,
                    self.workspaces.as_ref(),
                    self.rename_state.as_ref(),
                    None,
                );
                let surface_w = self
                    .gpu
                    .as_ref()
                    .map(|g| g.config.width as f32)
                    .unwrap_or(0.0);
                let csd_reserved = if self.config.general.use_csd {
                    crate::pane::CSD_BUTTONS_TOTAL_WIDTH
                } else {
                    0.0
                };
                if let Some(ws_id) = chrome::tab_bar_hit_test(
                    px as f32,
                    &workspace_ids,
                    &tab_labels,
                    surface_w,
                    csd_reserved,
                ) {
                    let bindings = &self.config.keybindings.bindings;
                    let menu = crate::menu::build_tab_menu(ws_id, bindings, (px as f32, py as f32));
                    self.active_menu = Some(menu);
                    self.tab_menu_workspace_id = Some(ws_id);
                }
            } else {
                self.open_context_menu(px as f32, py as f32);
            }
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
            let workspace_count = self.workspaces.as_ref().map(|wm| wm.len()).unwrap_or(1);
            let tab_bar_visible = crate::pane::should_show_tab_bar(workspace_count);
            let tab_bar_h = crate::pane::effective_tab_bar_height_csd(
                workspace_count,
                use_csd,
                self.focus_mode,
            );
            // tn-sfn9: focus mode hides the CSD/tab bar, skip hit-testing.
            if !self.focus_mode && (tab_bar_visible || use_csd) && (py as f32) < tab_bar_h {
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
                            chrome::CsdAction::Settings => {
                                self.open_settings_overlay();
                                if let Some(w) = self.window.as_ref() {
                                    w.request_redraw();
                                }
                                return;
                            }
                        }
                        return;
                    }
                }

                // Tab click: switch workspace.
                // tn-t2yd.4: under CSD a single-workspace tab is still
                // rendered, so we hit-test it here too. `tab_bar_hit_test`
                // handles the single-entry case natively.
                if tab_bar_visible || use_csd {
                    let workspace_ids = self
                        .workspaces
                        .as_ref()
                        .map(|wm| wm.workspace_ids())
                        .unwrap_or_default();
                    let tab_labels = super::build_tab_labels(
                        &workspace_ids,
                        self.workspaces.as_ref(),
                        self.rename_state.as_ref(),
                        None,
                    );
                    let surface_w = self
                        .gpu
                        .as_ref()
                        .map(|g| g.config.width as f32)
                        .unwrap_or(0.0);
                    let csd_reserved2 = if use_csd {
                        crate::pane::CSD_BUTTONS_TOTAL_WIDTH
                    } else {
                        0.0
                    };
                    if let Some(ws_id) = chrome::tab_bar_hit_test(
                        px as f32,
                        &workspace_ids,
                        &tab_labels,
                        surface_w,
                        csd_reserved2,
                    ) {
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
                    let is_double = self
                        .last_tab_bar_click
                        .is_some_and(|t| now.duration_since(t) < Duration::from_millis(300));
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

        // ── Status bar click: agent indicator → tail event log ─────
        if state == ElementState::Pressed
            && button == MouseButton::Left
            && let Some((px, py)) = self.cursor_position
            && let Some(hit) =
                chrome::status_bar_hit_test(px as f32, py as f32, &self.status_bar_hit_areas)
        {
            match hit {
                chrome::StatusBarHit::AgentIndicator => {
                    self.open_focused_agent_event_log_tail();
                }
            }
            return;
        }

        // ── Window edge resize (CSD borderless windows) ────────────
        // Runs after tab bar / status bar / context menu handling so
        // existing UI elements always win. Mouse-down on an outer
        // window edge hands off to the compositor via winit's
        // `drag_resize_window`.
        if state == ElementState::Pressed
            && button == MouseButton::Left
            && self.config.general.use_csd
            && self.active_menu.is_none()
            && let Some((px, py)) = self.cursor_position
            && self.try_start_edge_resize(px as f32, py as f32)
        {
            return;
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
                HeaderAction::Zoom(pane_id) => {
                    // Focus the pane first so zoom acts on the clicked pane.
                    self.set_focused_pane(Some(pane_id));
                    self.zoom_toggle_focused_pane();
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

    /// Begin an inline rename of the given workspace tab.
    pub(super) fn start_rename_workspace(&mut self, workspace_id: usize) {
        // Seed the edit buffer with the custom name only. The tab number
        // prefix is always rendered by build_tab_labels, so starting with
        // the id preloaded would force the user to delete it before typing.
        let initial = self
            .workspaces
            .as_ref()
            .and_then(|wm| wm.name_for(workspace_id))
            .map(|s| s.to_string())
            .filter(|s| s != &workspace_id.to_string())
            .unwrap_or_default();
        self.rename_state = Some(super::RenameState::new(workspace_id, initial));
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Commit the current rename buffer to the workspace manager and exit rename mode.
    pub(super) fn commit_rename(&mut self) {
        let Some(state) = self.rename_state.take() else {
            return;
        };
        let trimmed = state.buffer.trim();
        let new_name = if trimmed.is_empty() {
            // Empty name resets to numeric default.
            state.workspace_id.to_string()
        } else {
            trimmed.to_string()
        };
        if let Some(wm) = self.workspaces.as_mut() {
            wm.rename(state.workspace_id, new_name);
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
        // F12 (tn-97j6): if any pane is local-only (Phase B mixed mode),
        // publish_workspace_state silently no-ops via the translation guard
        // and the rename never reaches the daemon. Surface this to the user
        // so they know MCP/persistence won't see the new name.
        if !self.publish_workspace_state() {
            tracing::warn!(
                workspace_id = state.workspace_id,
                "workspace rename committed locally but did not reach the daemon (mixed local/remote pane ids — Phase B incomplete)"
            );
            self.show_toast("rename: daemon not updated (mixed-mode panes)");
        }
    }

    /// Cancel the in-progress rename without applying changes.
    pub(super) fn cancel_rename(&mut self) {
        self.rename_state = None;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Handle a keyboard event while inline rename mode is active.
    /// Esc cancels, Enter commits, Backspace deletes, character keys insert.
    fn handle_rename_key(&mut self, key_event: &KeyEvent) {
        match &key_event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.cancel_rename();
                return;
            }
            Key::Named(NamedKey::Enter) => {
                self.commit_rename();
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(state) = self.rename_state.as_mut() {
                    state.backspace();
                }
            }
            Key::Character(s) => {
                if let Some(state) = self.rename_state.as_mut() {
                    for c in s.chars() {
                        if !c.is_control() {
                            state.insert_char(c);
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    pub(super) fn handle_keyboard_input_event(&mut self, key_event: &KeyEvent) {
        // ── Inline workspace rename input ──────────────────────
        if self.rename_state.is_some() {
            self.handle_rename_key(key_event);
            return;
        }

        // ── Context menu keyboard navigation ───────────────────
        if self.active_menu.is_some() {
            match &key_event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.active_menu = None;
                }
                Key::Named(NamedKey::ArrowUp) => {
                    if let Some(m) = self.active_menu.as_mut() {
                        m.move_up();
                    }
                }
                Key::Named(NamedKey::ArrowDown) => {
                    if let Some(m) = self.active_menu.as_mut() {
                        m.move_down();
                    }
                }
                Key::Named(NamedKey::Enter) => {
                    self.execute_menu_action();
                }
                // Modifier-only keys (Shift, Ctrl, Alt, Super) should not
                // dismiss the menu — Shift+Click opens the hotspot palette and
                // the Shift release arrives as a key event after the menu opens.
                Key::Named(
                    NamedKey::Shift | NamedKey::Control | NamedKey::Alt | NamedKey::Super,
                ) => {}
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

        // Overlay key handling runs before general keybindings.
        match self.overlay_mode {
            Some(OverlayMode::Help) => {
                match &key_event.logical_key {
                    Key::Named(NamedKey::Escape) => self.close_overlay(),
                    Key::Named(NamedKey::ArrowUp) => self.scroll_help_overlay_by(-1),
                    Key::Named(NamedKey::ArrowDown) => self.scroll_help_overlay_by(1),
                    Key::Named(NamedKey::PageUp) => self.scroll_help_overlay_by(-10),
                    Key::Named(NamedKey::PageDown) => self.scroll_help_overlay_by(10),
                    Key::Named(NamedKey::Home) => self.help_overlay_scroll_rows = 0,
                    Key::Named(NamedKey::End) => {
                        self.help_overlay_scroll_rows = self.help_overlay_max_scroll_rows();
                    }
                    Key::Character(s) if s.eq_ignore_ascii_case("j") => {
                        self.scroll_help_overlay_by(1)
                    }
                    Key::Character(s) if s.eq_ignore_ascii_case("k") => {
                        self.scroll_help_overlay_by(-1)
                    }
                    _ => self.close_overlay(),
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return;
            }
            Some(OverlayMode::Settings) => {
                let is_editing = self.settings_overlay.is_text_editing();
                let is_select_expanded = self.settings_overlay.is_select_expanded();
                match &key_event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        if !self.settings_overlay.cancel_text_editing()
                            && !self.settings_overlay.cancel_select_expanded()
                        {
                            self.close_overlay();
                        }
                    }
                    Key::Named(NamedKey::Tab) if !is_editing && !is_select_expanded => {
                        let reverse = self.modifiers.state().shift_key();
                        self.settings_overlay.tab(reverse);
                    }
                    Key::Named(NamedKey::ArrowUp) if !is_editing => {
                        self.settings_overlay.arrow_up();
                    }
                    Key::Named(NamedKey::ArrowDown) if !is_editing => {
                        self.settings_overlay.arrow_down();
                    }
                    Key::Named(NamedKey::ArrowLeft) => self.settings_overlay.arrow_left(),
                    Key::Named(NamedKey::ArrowRight) => self.settings_overlay.arrow_right(),
                    Key::Named(NamedKey::Enter) => {
                        if let Some(cmd) = self.settings_overlay.enter() {
                            self.apply_settings_command(cmd);
                        }
                    }
                    Key::Named(NamedKey::Space) if !is_editing => {
                        if let Some(cmd) = self.settings_overlay.space() {
                            self.apply_settings_command(cmd);
                        }
                    }
                    Key::Named(NamedKey::Backspace) => {
                        self.settings_overlay.backspace();
                    }
                    Key::Named(NamedKey::Delete) => {
                        if let Some(cmd) = self.settings_overlay.delete() {
                            self.apply_settings_command(cmd);
                        }
                    }
                    Key::Character(s) if is_editing => {
                        for ch in s.chars() {
                            self.settings_overlay.char_input(ch);
                        }
                    }
                    _ => {}
                }
                self.settings_overlay
                    .sync_toggle_values(&self.build_settings_render_values());
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return;
            }
            Some(OverlayMode::TrustEscalation) => {
                match &key_event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        self.resolve_trust_escalation(false);
                    }
                    Key::Named(NamedKey::Enter) => {
                        if let Some(ref state) = self.trust_escalation {
                            let approved = state.approve_focused;
                            self.resolve_trust_escalation(approved);
                        }
                    }
                    Key::Named(NamedKey::Tab)
                    | Key::Named(NamedKey::ArrowLeft)
                    | Key::Named(NamedKey::ArrowRight) => {
                        if let Some(ref mut state) = self.trust_escalation {
                            state.toggle_focus();
                        }
                    }
                    _ => {}
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return;
            }
            None => {}
        }

        // Check configured keybindings first.
        if self.handle_keybinding(key_event) {
            // Keybinding consumed the event. Copy/Paste preserve
            // selection; other actions clear it.
            let action = lookup_binding(&self.binding_map, &self.modifiers, key_event);
            let preserves = matches!(action, Some(KeyAction::Copy) | Some(KeyAction::Paste));
            if !preserves {
                self.clear_selection();
            }
        } else {
            // Regular keypress clears any active selection.
            self.clear_selection();
            self.handle_key_input(key_event);
        }
    }
}
