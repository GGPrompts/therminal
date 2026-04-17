//! Window event dispatch helpers.
//!
//! `App::window_event` in `mod.rs` is now a thin match that delegates each
//! variant to a `pub(super) handle_*` method on this `impl App` block.

mod pty_input;
mod scroll;
mod settings;

use std::time::{Duration, Instant};

use tracing::{info, warn};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};

use therminal_core::config::KeyAction;

use super::keybindings::lookup_binding;
use super::mouse::HeaderAction;
use super::render_driver::JumpDirection;
use super::{App, NavigateMode, NavigateState, OverlayMode, chrome, help_overlay};
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

    /// Check if this key event matches a configured keybinding.
    /// Returns true if the event was consumed.
    pub(super) fn handle_keybinding(&mut self, key_event: &KeyEvent) -> bool {
        let action = match lookup_binding(&self.binding_map, &self.modifiers, key_event) {
            Some(a) => a,
            None => return false,
        };

        // tn-wvll: NavigateWebView is WebView-pane-only. Falling through
        // (returning false) when the focused pane isn't a WebView lets
        // terminal panes still bind ctrl+l to readline clear-screen.
        if matches!(action, KeyAction::NavigateWebView) {
            let pane_id = match self.focused_pane() {
                Some(id) => id,
                None => return false,
            };
            let is_webview = self
                .get_layout()
                .and_then(|l| l.find_pane(pane_id))
                .map(|p| p.is_webview())
                .unwrap_or(false);
            if !is_webview {
                return false;
            }
            self.start_navigate_webview(pane_id);
            return true;
        }

        // tn-ojy9: SpawnWebViewPane opens an inline URL input; on commit a
        // NEW WebView pane is created, splitting off the focused pane.
        // Unlike NavigateWebView, this works regardless of the focused
        // pane's backend.
        if matches!(action, KeyAction::SpawnWebViewPane) {
            let pane_id = match self.focused_pane() {
                Some(id) => id,
                None => return false,
            };
            self.start_spawn_webview(pane_id);
            return true;
        }

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
            KeyAction::ShowLauncher => {
                // The close branch is handled by the overlay key handler
                // (line ~1199), which catches all keys — including the
                // ShowLauncher binding — and returns before handle_keybinding
                // runs. So we only need the open path here.
                self.open_launcher_overlay();
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
            KeyAction::TogglePinPane => {
                // tn-n5jk: toggle the pinned state of the focused pane.
                self.toggle_pin_focused_pane();
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
            // tn-5dpv: scrollback navigation
            KeyAction::ScrollPageUp => {
                self.scroll_focused_pane(alacritty_terminal::grid::Scroll::PageUp);
            }
            KeyAction::ScrollPageDown => {
                self.scroll_focused_pane(alacritty_terminal::grid::Scroll::PageDown);
            }
            KeyAction::ScrollTop => {
                self.scroll_focused_pane(alacritty_terminal::grid::Scroll::Top);
            }
            KeyAction::ScrollBottom => {
                self.scroll_focused_pane(alacritty_terminal::grid::Scroll::Bottom);
            }
            // NavigateWebView is handled above by the early-return guard
            // so it can fall through when the focused pane isn't a
            // WebView. If we reach this match arm it means the guard
            // approved the action; the arm is unreachable in practice.
            KeyAction::NavigateWebView => {}
            // tn-ojy9: SpawnWebViewPane is handled above by an early-return
            // guard. Unreachable in practice.
            KeyAction::SpawnWebViewPane => {}
            // Hotspot actions are menu-only; they shouldn't reach keybinding dispatch.
            KeyAction::HotspotCopy(_)
            | KeyAction::HotspotOpenInEditor(_)
            | KeyAction::HotspotOpenExternal(_)
            | KeyAction::HotspotOpenFolderInPane(_)
            | KeyAction::HotspotOpenFolderInFileManager(_)
            | KeyAction::HotspotShowGitRef { .. }
            | KeyAction::HotspotOpenUrlInPane(_)
            | KeyAction::OpenInBrowser(_) => {}
        }
        true
    }

    /// Open a context menu at the given pixel position, or pass through to
    /// the PTY if the pane has mouse reporting enabled (like tmux does).
    ///
    /// `force_show` bypasses the TUI mouse-reporting passthrough so the
    /// therminal context menu always opens. Wired up for Shift+Right-Click
    /// (tn-gm6f) as a universal escape hatch when the TUI or WebView would
    /// normally consume the click.
    pub(super) fn open_context_menu(&mut self, px: f32, py: f32, force_show: bool) {
        let pane_id = match self.pane_at_position(px as f64, py as f64) {
            Some(id) => id,
            None => return,
        };

        // Smart pass-through: if the terminal app has mouse reporting enabled,
        // forward the right-click to the PTY instead of showing our menu.
        // This lets TUI apps like btop, lazygit, etc. handle their own menus.
        // `force_show` (Shift+Right-Click) bypasses this so the user can always
        // reach the therminal menu — useful when the TUI menu is broken or
        // they want pane-level actions like Split/Close.
        let mode = self.pane_term_mode(pane_id);
        let mouse_mode = mode.contains(alacritty_terminal::term::TermMode::MOUSE_REPORT_CLICK)
            || mode.contains(alacritty_terminal::term::TermMode::SGR_MOUSE)
            || mode.contains(alacritty_terminal::term::TermMode::MOUSE_DRAG)
            || mode.contains(alacritty_terminal::term::TermMode::MOUSE_MOTION);

        if mouse_mode && !force_show {
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

        // tn-s5vj: WebView panes get a dedicated context menu with
        // "Open in browser" + "Copy URL" instead of terminal actions.
        let is_webview_pane = self
            .get_layout()
            .and_then(|l| l.find_pane(pane_id))
            .map(|p| p.is_webview())
            .unwrap_or(false);

        let mut menu = if is_webview_pane {
            let (url, is_pinned) = self
                .get_layout()
                .and_then(|l| l.find_pane(pane_id))
                .map(|p| {
                    (
                        p.webview_url().map(|u| u.to_string()).unwrap_or_default(),
                        p.pinned,
                    )
                })
                .unwrap_or_default();
            crate::menu::build_webview_pane_menu(pane_id, &url, is_pinned, bindings, (px, py))
        } else if has_selection {
            let text = self
                .get_layout()
                .and_then(|l| l.find_pane(pane_id))
                .and_then(|p| p.backend.term())
                .and_then(|t| t.lock().selection_to_string())
                .unwrap_or_default();
            crate::menu::build_selection_menu(text, bindings, (px, py))
        } else {
            {
                let is_pinned = self
                    .get_layout()
                    .and_then(|l| l.find_pane(pane_id))
                    .map(|p| p.pinned)
                    .unwrap_or(false);
                crate::menu::build_pane_menu(pane_id, is_pinned, bindings, (px, py))
            }
        };

        // If a hotspot sits under the cursor, prepend hotspot-specific
        // action sections so both the hotspot actions and the generic pane
        // actions are available in a single merged menu.
        //
        // URL hyperlinks are a special case (tn-t0gp): regex-detected
        // URLs carry the hyperlink bit but are intentionally excluded
        // from `hotspot_map` in `process_hotspots` so they don't render a
        // second dotted underline on top of the existing colored one. So
        // we fall back to `hyperlink_map` and synthesise a URL hotspot
        // palette when the cell holds an `http(s)://` hyperlink.
        if !has_selection
            && let Some((col, row)) = self.pixel_to_grid_for_pane(px as f64, py as f64, pane_id)
        {
            let hotspot_entry = self
                .grid_renderer
                .as_ref()
                .and_then(|r| r.hotspot_map.get(&(pane_id, row, col)).cloned());

            let hotspot_menu = if let Some((kind, text, is_dir, resolved)) = hotspot_entry {
                // Prefer the harness-resolved absolute path when present
                // (tn-gidy) so right-click menu actions also survive agent
                // worktree hops.
                let effective = resolved.unwrap_or(text);
                Some(crate::menu::build_hotspot_palette(
                    kind,
                    effective.to_string(),
                    is_dir,
                    &self.discovered_git_tools,
                    (px, py),
                ))
            } else {
                self.grid_renderer
                    .as_ref()
                    .and_then(|r| r.hyperlink_map.get(&(pane_id, row, col)).cloned())
                    .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
                    .map(|url| {
                        crate::menu::build_hotspot_palette(
                            therminal_terminal::hotspot_detection::HotspotKind::Url,
                            url.to_string(),
                            false,
                            &self.discovered_git_tools,
                            (px, py),
                        )
                    })
            };

            if let Some(hotspot_menu) = hotspot_menu {
                let mut merged = hotspot_menu.sections;
                merged.append(&mut menu.sections);
                menu.sections = merged;
            }
        }

        self.active_menu = Some(menu);

        // Proactively hide any visible webview child HWNDs the moment a
        // menu opens. The render-driver visibility logic would hide them
        // on the next redraw, but on Windows the webview child HWND can
        // keep capturing mouse input in its old bounds until `ShowWindow`
        // is actually flushed. Hiding synchronously here avoids menu
        // items overlapping a visible webview from being click-stolen.
        if !self.webview_manager.pane_ids().is_empty() {
            self.webview_manager.hide_all();
        }
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
            KeyAction::TogglePinPane => self.toggle_pin_focused_pane(),
            KeyAction::OpenInBrowser(ref url) => {
                if let Err(e) = open::that(url) {
                    warn!("failed to open URL in browser {url}: {e}");
                    self.show_toast(format!("Failed to open in browser: {e}"));
                }
            }
            KeyAction::HotspotOpenUrlInPane(ref url) => {
                // tn-t0gp: open URL in a new WebView pane split off the
                // menu's pane context (right-click originator) or the
                // currently focused pane. `create_webview_pane` splits
                // from the focused pane, so ensure focus is on the
                // originator first. Runs on the main thread — no IPC
                // round-trip (tn-ojy9 precedent).
                let source_pane = menu_pane_id.or_else(|| self.focused_pane());
                if let Some(id) = source_pane {
                    self.set_focused_pane(Some(id));
                }
                if let Err(e) = self.create_webview_pane(url) {
                    warn!(url = %url, error = %e, "failed to spawn webview pane for URL hotspot");
                    self.show_toast(format!("WebView spawn failed: {e}"));
                }
            }
            KeyAction::NavigateWebView => {
                // tn-wvll: menu route uses the menu's pane context; the
                // keybinding route uses the focused pane (via
                // handle_keybinding's early-return). Either way we open
                // the inline input over the targeted WebView pane.
                let pane_id = menu_pane_id.or_else(|| self.focused_pane());
                if let Some(id) = pane_id {
                    let is_webview = self
                        .get_layout()
                        .and_then(|l| l.find_pane(id))
                        .map(|p| p.is_webview())
                        .unwrap_or(false);
                    if is_webview {
                        self.start_navigate_webview(id);
                    }
                }
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
        // tn-rl6i + tn-6061: on Windows, minimizing normally sends
        // `Resized(0, 0)`, but with CSD enabled (therminal's default) some
        // style combinations report a small-but-nonzero inner size instead
        // (seen in practice with client-decorated frames). The zero-only
        // check leaked those events through to `self.resize()`, which
        // re-flowed every PTY down to ~3 cols and garbled any TUI (Claude
        // Code especially) for the duration of the un-minimize. Prefer
        // winit's `Window::is_minimized()` where available; fall back to a
        // `< 80 × 80` heuristic (1 px shy of a comfortable terminal) so
        // platforms that return `None` from `is_minimized()` still get the
        // correct treatment.
        let winit_says_minimized = self
            .window
            .as_ref()
            .and_then(|w| w.is_minimized())
            .unwrap_or(false);
        let heuristic_minimized = new_size.width < 80 || new_size.height < 80;
        if winit_says_minimized || heuristic_minimized {
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
            // Keep launcher grid cols in sync with the new surface width.
            if self.overlay_mode == Some(OverlayMode::Launcher) {
                self.launcher_state.cols = super::launcher_overlay::compute_cols(
                    self.launcher_state.entries.len(),
                    new_size.width as f32,
                );
            }
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
        // tn-6061: the scale-factor path previously had no minimize guard
        // and called `self.resize()` unconditionally. On Windows a DPI
        // change event can fire while the window is minimized (e.g. when
        // the user drags a minimized app to a different-DPI monitor) with
        // a tiny `inner_size()`, which would then shrink every PTY. Mirror
        // the guards in `handle_resized`: skip while flagged minimized and
        // short-circuit on zero dimensions before touching `self.resize`.
        if self.minimized {
            return;
        }
        let new_size = self.window.as_ref().map(|w| w.inner_size());
        if let Some(size) = new_size {
            if size.width == 0 || size.height == 0 {
                return;
            }
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
            // Keep launcher grid cols in sync with the new surface width.
            if self.overlay_mode == Some(OverlayMode::Launcher) {
                self.launcher_state.cols = super::launcher_overlay::compute_cols(
                    self.launcher_state.entries.len(),
                    size.width as f32,
                );
            }
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
            Some(OverlayMode::TrustEscalation) | Some(OverlayMode::Launcher) => {
                // No scrolling for trust escalation or launcher modal.
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

        // tn-wvll: any mouse press while the WebView navigate input is
        // active cancels it. Unlike rename, we cancel rather than commit
        // — an accidental click shouldn't navigate the page to a partial
        // URL the user is still typing.
        if self.navigate_state.is_some() && state == ElementState::Pressed {
            self.cancel_navigate();
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
                let force_show = self.modifiers.state().shift_key();
                self.open_context_menu(px as f32, py as f32, force_show);
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
            {
                let internal_focus_changed = self.focused_pane() != Some(pane_id);
                if internal_focus_changed {
                    self.set_focused_pane(Some(pane_id));
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                } else {
                    // tn-shgq: click landed in the already-internally-focused
                    // pane, so set_focused_pane (and its tn-0xuo focus_window
                    // hook) is skipped. If the user clicked a WebView earlier,
                    // OS keyboard focus is stranded on that child HWND; this
                    // click is the natural moment to reclaim it for a
                    // non-WebView pane. Clicks inside a WebView pane's content
                    // never reach winit (the child HWND captures them), so
                    // this only fires when the click landed on a terminal
                    // pane's content or drawn area.
                    if let Some(layout) = self.workspaces.as_ref().map(|wm| wm.layout())
                        && let Some(pane) = layout.find_pane(pane_id)
                        && !pane.is_webview()
                        && let Some(w) = self.window.as_ref()
                    {
                        crate::window::restore_main_window_focus(w);
                    }
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

    // ── WebView Navigate input (tn-wvll) ─────────────────────────────────

    /// Open the inline URL input over the given WebView pane, pre-filled
    /// with the pane's current URL so the user can edit rather than
    /// retype. No-op when the pane is not a WebView.
    pub(super) fn start_navigate_webview(&mut self, pane_id: crate::pane::PaneId) {
        let is_webview = self
            .get_layout()
            .and_then(|l| l.find_pane(pane_id))
            .map(|p| p.is_webview())
            .unwrap_or(false);
        if !is_webview {
            return;
        }
        // Prefer the runtime URL from the wry view (reflects post-navigate
        // state); fall back to the backend's stored URL (set at create()
        // time and updated by `commit_navigate`).
        let initial = self
            .webview_manager
            .url(pane_id)
            .or_else(|| {
                self.get_layout()
                    .and_then(|l| l.find_pane(pane_id))
                    .and_then(|p| p.webview_url().map(|u| u.to_string()))
            })
            .unwrap_or_default();
        self.navigate_state = Some(NavigateState::new(pane_id, initial));
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Open the inline URL input to spawn a NEW WebView pane off the given
    /// source pane (tn-ojy9). The source pane is typically the focused pane
    /// at the moment `Ctrl+Shift+B` was pressed; the new WebView will split
    /// off it when the user commits a URL. Unlike [`start_navigate_webview`],
    /// the source pane does NOT have to be a WebView — any backend works.
    pub(super) fn start_spawn_webview(&mut self, source_pane_id: crate::pane::PaneId) {
        self.navigate_state = Some(NavigateState::new_spawn(source_pane_id, String::new()));
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Commit the navigate buffer. Behavior depends on
    /// [`NavigateState::mode`]:
    ///
    /// - [`NavigateMode::Navigate`] (tn-wvll): route the URL through the
    ///   WebViewManager and update the pane backend's stored URL so
    ///   subsequent reads (menu "Copy URL", header label, future pre-fill)
    ///   see the new value.
    /// - [`NavigateMode::Spawn`] (tn-ojy9): spawn a new WebView pane split
    ///   off the stored source pane id. `create_webview_pane` picks the
    ///   split direction automatically from the source pane viewport.
    ///
    /// Empty submit cancels (treated identically to Esc).
    pub(super) fn commit_navigate(&mut self) {
        let Some(state) = self.navigate_state.take() else {
            return;
        };
        let url = state.buffer.trim().to_string();
        if url.is_empty() {
            // Empty submit → cancel without acting.
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
            return;
        }
        match state.mode {
            NavigateMode::Navigate => {
                self.webview_manager.navigate(state.pane_id, &url);
                if let Some(layout) = self.get_layout_mut()
                    && let Some(pane) = layout.find_pane_mut(state.pane_id)
                {
                    pane.set_webview_url(url);
                }
            }
            NavigateMode::Spawn => {
                // tn-ojy9: spawn the new WebView pane splitting off the
                // stored source pane. `create_webview_pane` uses the
                // focused pane as the split source, so ensure focus is
                // on the recorded source pane first.
                self.set_focused_pane(Some(state.pane_id));
                if let Err(e) = self.create_webview_pane(&url) {
                    warn!(url = %url, error = %e, "failed to spawn webview pane");
                    self.show_toast(format!("WebView spawn failed: {e}"));
                }
            }
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Cancel the in-progress navigation without applying changes.
    pub(super) fn cancel_navigate(&mut self) {
        self.navigate_state = None;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Handle a keyboard event while inline navigate mode is active.
    /// Esc cancels, Enter commits, Backspace deletes, character keys insert.
    fn handle_navigate_key(&mut self, key_event: &KeyEvent) {
        match &key_event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.cancel_navigate();
                return;
            }
            Key::Named(NamedKey::Enter) => {
                self.commit_navigate();
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(state) = self.navigate_state.as_mut() {
                    state.backspace();
                }
            }
            Key::Character(s) => {
                if let Some(state) = self.navigate_state.as_mut() {
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

        // ── Inline WebView navigate input (tn-wvll) ────────────
        if self.navigate_state.is_some() {
            self.handle_navigate_key(key_event);
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
                    Key::Named(NamedKey::Space) if is_editing => {
                        self.settings_overlay.char_input(' ');
                    }
                    Key::Named(NamedKey::Space) => {
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
            Some(OverlayMode::Launcher) => {
                match &key_event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        self.close_overlay();
                    }
                    Key::Named(NamedKey::ArrowUp) => {
                        self.launcher_state.navigate(0, -1);
                    }
                    Key::Named(NamedKey::ArrowDown) => {
                        self.launcher_state.navigate(0, 1);
                    }
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.launcher_state.navigate(-1, 0);
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        self.launcher_state.navigate(1, 0);
                    }
                    Key::Named(NamedKey::Enter) => {
                        self.launch_selected_profile();
                    }
                    _ => {
                        // Any other key closes the launcher.
                        self.close_overlay();
                    }
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
            // tn-zhac: Non-PTY backends (JsonlTail) handle input in-place
            // without a PTY echo loop, so nothing else triggers a redraw.
            // Request one explicitly. For terminal panes this is harmless
            // because winit coalesces redundant redraw requests with the
            // PTY-reader-driven redraw that follows.
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }
}
