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

use therminal_core::config::KeyAction;
use therminal_terminal::input::{self, KeyCode, Modifiers as InputModifiers};

use super::keybindings::lookup_binding;
use super::mouse::HeaderAction;
use super::render_driver::JumpDirection;
use super::{App, chrome};
use crate::pane::SplitDirection;

impl App {
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
            // Hotspot actions are menu-only; they shouldn't reach keybinding dispatch.
            KeyAction::HotspotCopy(_)
            | KeyAction::HotspotOpenInEditor(_)
            | KeyAction::HotspotOpenExternal(_)
            | KeyAction::HotspotOpenFolderInPane(_)
            | KeyAction::HotspotOpenFolderInFileManager(_) => {}
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
            let hotspot_menu =
                crate::menu::build_hotspot_palette(kind, effective.to_string(), is_dir, (px, py));
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
            // tn-ou30: the first authoritative `Resized` arrives shortly
            // after window creation. If the local-mode initial pane spawn
            // was deferred (Windows DPI/DWM-reshape race), spawn it now —
            // *after* `self.resize` has committed the new surface dims so
            // `compute_layout_rect` returns the real rect.
            self.ensure_initial_local_pane_spawned();
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
        self.ensure_initial_local_pane_spawned();

        // workspaces == None means all panes are gone and no restore
        // is pending — exit the window. Gate on `initial_pane_pending`
        // (tn-ou30) so we don't exit during the brief window between
        // `init_gpu` returning and the deferred initial spawn landing.
        if self.workspaces.is_none() && !self.initial_pane_pending {
            event_loop.exit();
            return;
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

        // Dismiss help overlay on any mouse click.
        if self.show_help_overlay && state == ElementState::Pressed && button == MouseButton::Left {
            self.show_help_overlay = false;
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
            let show_tab_bar = self.config.general.show_tab_bar;
            let use_csd = self.config.general.use_csd;
            let tab_bar_h = crate::pane::effective_tab_bar_height_csd(show_tab_bar, use_csd);
            if show_tab_bar && (py as f32) < tab_bar_h {
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
                );
                let surface_w = self
                    .gpu
                    .as_ref()
                    .map(|g| g.config.width as f32)
                    .unwrap_or(0.0);
                if let Some(ws_id) =
                    chrome::tab_bar_hit_test(px as f32, &workspace_ids, &tab_labels, surface_w)
                {
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
            let show_tab_bar = self.config.general.show_tab_bar;
            let tab_bar_h = crate::pane::effective_tab_bar_height_csd(show_tab_bar, use_csd);
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
                            chrome::CsdAction::Settings => {
                                let config_file = therminal_core::config::config_path();
                                if !config_file.exists()
                                    && let Err(e) =
                                        therminal_core::config::TherminalConfig::default()
                                            .save_default_to(&config_file)
                                {
                                    tracing::warn!(
                                        "settings: failed to create default config at {}: {e}",
                                        config_file.display()
                                    );
                                    self.show_toast(format!(
                                        "failed to create {}",
                                        config_file.display()
                                    ));
                                    return;
                                }
                                // Use the absolute-path editor hand-off,
                                // not `open_in_editor`. The latter joins
                                // the path against the focused pane's
                                // shell cwd, which on Windows+WSL turns
                                // `C:\Users\…\therminal.toml` into
                                // `/home/marci/…/C:\Users\…\therminal.toml`.
                                self.open_absolute_in_editor(&config_file);
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
                    let tab_labels = super::build_tab_labels(
                        &workspace_ids,
                        self.workspaces.as_ref(),
                        self.rename_state.as_ref(),
                    );
                    let surface_w = self
                        .gpu
                        .as_ref()
                        .map(|g| g.config.width as f32)
                        .unwrap_or(0.0);
                    if let Some(ws_id) =
                        chrome::tab_bar_hit_test(px as f32, &workspace_ids, &tab_labels, surface_w)
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
