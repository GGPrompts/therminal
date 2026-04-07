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
mod event_handler;
mod help_overlay;
mod init;
mod keybindings;
mod mouse;
mod pane_ops;
mod render;
mod render_driver;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::grid_renderer::{FontConfig, GridRenderer};
use crate::menu::ContextMenu;
use crate::pane::{AutoTileDebouncer, LayoutNode, PaneId, SplitDirection, WorkspaceManager};
use therminal_core::config::{KeyAction, TherminalConfig};
use therminal_core::config_watcher::ConfigWatcher;
use therminal_core::font::PLATFORM_MONOSPACE;
use therminal_core::geometry::Rect;

use keybindings::{BindingLookup, build_binding_map};

// ── Custom event for waking the event loop from the PTY reader ───────────

/// Events sent from background threads to the winit event loop.
#[derive(Debug)]
enum UserEvent {
    /// New bytes are available from a pane's PTY; request a redraw.
    PtyOutput,
    /// A pane's PTY has closed (shell exited); remove the pane.
    PaneExited(crate::pane::PaneId),
    /// Config file changed; apply new settings.
    ConfigChanged(Box<therminal_core::config_watcher::ConfigChanged>),
    /// A BEL character was received from a pane.
    Bell(crate::pane::PaneId),
    /// A desktop notification was requested (OSC 9 or agent event).
    DesktopNotification {
        title: String,
        body: String,
        source: NotificationSource,
    },
    /// The swarm watcher bridge has new events queued in the
    /// `SwarmDebouncer`. Triggers a `poll_swarm_watcher` pass on the main
    /// thread; actual spawn/reclaim happens once the debounce window expires.
    SwarmWatcherTick,
}

/// Origin of a desktop notification request, used to apply per-source
/// config gating (`[notifications]` section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSource {
    /// Triggered by an OSC 9 escape sequence from a pane.
    Osc9,
    /// Triggered by an agent state change (e.g. `AwaitingInput`).
    Agent,
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

    /// Shared agent registry for auto-tiling (reader threads register/unregister agents).
    agent_registry: Arc<std::sync::Mutex<therminal_terminal::agent_registry::AgentRegistry>>,

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

    /// Workspace id whose tab the user right-clicked to open the active tab
    /// context menu. Used so menu actions like Rename know which tab to act on.
    pub(crate) tab_menu_workspace_id: Option<usize>,

    /// In-progress inline workspace rename, if any.
    pub(crate) rename_state: Option<RenameState>,

    /// Active separator drag state (path to split node, direction, parent rect).
    separator_drag: Option<SeparatorDrag>,

    /// Whether the cursor is currently showing a resize icon (for separator hover).
    separator_cursor_active: bool,

    /// Whether the cursor is currently showing a pointer icon (for hyperlink hover).
    hyperlink_cursor_active: bool,

    /// Whether the cursor is currently showing a window edge resize icon (CSD only).
    pub(crate) edge_cursor_active: bool,

    /// Timestamp of last separator click (for double-click detection).
    last_separator_click: Option<Instant>,

    /// Timestamp of last tab bar click (for CSD double-click-to-maximize).
    last_tab_bar_click: Option<Instant>,

    /// Cooldown timestamp for destructive actions (close pane/window) to prevent
    /// double-close from keyboard repeat firing two events in the same batch.
    last_close_action: Option<Instant>,

    /// Auto-tile debouncer for agent spawn/exit events.
    auto_tile_debouncer: Option<AutoTileDebouncer>,

    /// Debouncer for swarm watcher events. A spawn followed by a reclaim
    /// within the debounce window cancels both, so subagents that finish
    /// quickly don't briefly flash a pane onto the screen.
    pub(crate) swarm_debouncer: Option<crate::pane::auto_tile::SwarmDebouncer>,

    /// Map of subagent agent_id -> pane_id for panes spawned by the swarm
    /// watcher, so reclaim events can find the right pane to close.
    pub(crate) swarm_panes: HashMap<String, PaneId>,

    /// Timestamp when a visual bell flash started (for timed invert effect).
    visual_bell_start: Option<Instant>,

    /// Pre-zoom layout tree, stored when a pane is zoomed to fullscreen.
    /// Contains the full layout with the zoomed pane replaced by `Empty`.
    zoomed_layout: Option<LayoutNode>,

    /// Hit-test areas captured from the most recent status bar render.
    /// Used by the mouse handler to detect clicks on chrome elements like
    /// the agent indicator.
    status_bar_hit_areas: chrome::StatusBarHitAreas,

    /// Transient label shown after a semantic region jump (region kind +
    /// optional command text), with the timestamp of when it was set.
    /// Currently logged via tracing; future work will surface it as an
    /// on-screen overlay toast.
    #[allow(dead_code)]
    region_jump_toast: Option<(String, Instant)>,
}

/// Compute the display labels for the workspace tab bar.
///
/// Free function (not a method) so callers that already hold a mutable
/// borrow on `App.grid_renderer` can still build labels without conflict.
///
/// Precedence per workspace:
/// 1. Inline rename in progress: show buffer + trailing cursor.
/// 2. Custom name set via rename: show `"<id>: <name>"`.
/// 3. Focused pane has a known cwd: show `"<id>: <basename>"`.
/// 4. Otherwise: just `"<id>"`.
pub(crate) fn build_tab_labels(
    workspace_ids: &[usize],
    workspaces: Option<&WorkspaceManager>,
    rename_state: Option<&RenameState>,
) -> Vec<String> {
    workspace_ids
        .iter()
        .map(|&ws_id| {
            if let Some(state) = rename_state
                && state.workspace_id == ws_id
            {
                return format!("{}: {}_", ws_id, state.buffer);
            }
            if let Some(name) = workspaces.and_then(|wm| wm.name_for(ws_id))
                && name != ws_id.to_string()
            {
                return format!("{ws_id}: {name}");
            }
            if let Some(status) = workspaces.and_then(|wm| wm.focused_pane_status(ws_id))
                && let Some(cwd) = status.cwd.as_ref()
            {
                let basename = std::path::Path::new(cwd)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(cwd);
                return format!("{ws_id}: {basename}");
            }
            format!("{ws_id}")
        })
        .collect()
}

/// State for an in-progress inline workspace tab rename.
#[derive(Debug, Clone)]
pub(crate) struct RenameState {
    /// Workspace ID being renamed.
    pub workspace_id: usize,
    /// Current edit buffer.
    pub buffer: String,
    /// Cursor position as a byte offset into `buffer` (always at a char boundary).
    pub cursor: usize,
}

impl RenameState {
    pub fn new(workspace_id: usize, initial: String) -> Self {
        let cursor = initial.len();
        Self {
            workspace_id,
            buffer: initial,
            cursor,
        }
    }

    /// Insert a character at the cursor and advance the cursor.
    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Find prev char boundary.
        let mut idx = self.cursor - 1;
        while idx > 0 && !self.buffer.is_char_boundary(idx) {
            idx -= 1;
        }
        self.buffer.replace_range(idx..self.cursor, "");
        self.cursor = idx;
    }
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
            || self.config.font.nerd_font != old_config.font.nerd_font
            || self.config.font.extra_fallbacks != old_config.font.extra_fallbacks
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
                let effective_family = if self.config.font.family.is_empty() {
                    PLATFORM_MONOSPACE.to_string()
                } else {
                    self.config.font.family.clone()
                };
                let mut new_font_config =
                    FontConfig::new(effective_family, self.config.font.size * scale);
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

    // ── Bell & notification handling ────────────────────────────────────

    /// Handle a BEL event from a pane according to the `[bell]` config.
    fn handle_bell(&mut self, pane_id: PaneId) {
        use therminal_core::config::BellStyle;

        debug!(pane_id, "bell received");
        match self.config.bell.style {
            BellStyle::Taskbar => {
                if let Some(w) = self.window.as_ref() {
                    w.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                }
            }
            BellStyle::Visual => {
                self.visual_bell_start = Some(Instant::now());
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            BellStyle::Audible => {
                // Audible bell: fall back to taskbar flash for now.
                if let Some(w) = self.window.as_ref() {
                    w.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                }
            }
            BellStyle::None => {}
        }
    }

    /// Send a desktop notification via notify-rust.
    fn send_desktop_notification(&self, title: &str, body: &str) {
        debug!(title, body, "sending desktop notification");
        let title = title.to_string();
        let body = body.to_string();
        // Fire-and-forget on a background thread to avoid blocking the event loop.
        std::thread::Builder::new()
            .name("desktop-notify".into())
            .spawn(move || {
                if let Err(e) = notify_rust::Notification::new()
                    .summary(&title)
                    .body(&body)
                    .appname("Therminal")
                    .show()
                {
                    warn!("failed to send desktop notification: {e}");
                }
            })
            .ok();
    }

    /// Check if the visual bell flash is still active and return the
    /// invert intensity (0.0 = off, 1.0 = full invert).
    #[allow(dead_code)]
    pub(crate) fn visual_bell_intensity(&self) -> f32 {
        let start = match self.visual_bell_start {
            Some(s) => s,
            None => return 0.0,
        };
        let elapsed_ms = start.elapsed().as_millis() as u64;
        let duration_ms = self.config.bell.visual_bell_duration_ms;
        if elapsed_ms >= duration_ms {
            return 0.0;
        }
        // Linear fade-out.
        1.0 - (elapsed_ms as f32 / duration_ms as f32)
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
            UserEvent::Bell(pane_id) => {
                self.handle_bell(pane_id);
            }
            UserEvent::SwarmWatcherTick => {
                // New raw events arrived from the swarm watcher bridge.
                // Drain the debouncer; expired events are dispatched to
                // spawn/reclaim. If anything is still pending, ask for a
                // redraw so the next poll happens after the debounce window.
                self.poll_swarm_watcher();
                if let Some(ref d) = self.swarm_debouncer
                    && d.has_pending()
                    && let Some(w) = self.window.as_ref()
                {
                    w.request_redraw();
                }
            }
            UserEvent::DesktopNotification {
                title,
                body,
                source,
            } => {
                // Gate OSC 9 notifications on `[notifications] osc9_enabled`.
                // The `terminal.osc_9` toggle only controls whether the
                // sequence is parsed; this field controls whether parsed
                // events trigger a desktop notification.
                if source == NotificationSource::Osc9 && !self.config.notifications.osc9_enabled {
                    debug!("OSC 9 notification suppressed by config");
                } else {
                    self.send_desktop_notification(&title, &body);
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
                self.handle_resized(new_size);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                info!("scale factor changed to {scale_factor:.2}");
                self.handle_scale_factor_changed();
            }
            WindowEvent::RedrawRequested => {
                self.handle_redraw_requested(event_loop);
            }
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.handle_cursor_moved_event(position);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_mouse_wheel_event(delta);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input_event(event_loop, state, button);
            }
            WindowEvent::KeyboardInput {
                event:
                    ref key_event @ KeyEvent {
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                self.handle_keyboard_input_event(key_event);
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

#[cfg(test)]
mod rename_state_tests {
    use super::RenameState;

    #[test]
    fn new_seeds_buffer_and_cursor_at_end() {
        let s = RenameState::new(2, "build".to_string());
        assert_eq!(s.workspace_id, 2);
        assert_eq!(s.buffer, "build");
        assert_eq!(s.cursor, 5);
    }

    #[test]
    fn insert_char_appends_at_cursor() {
        let mut s = RenameState::new(1, "ab".to_string());
        s.insert_char('c');
        assert_eq!(s.buffer, "abc");
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn backspace_removes_prev_char() {
        let mut s = RenameState::new(1, "abc".to_string());
        s.backspace();
        assert_eq!(s.buffer, "ab");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut s = RenameState::new(1, String::new());
        s.backspace();
        assert_eq!(s.buffer, "");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn backspace_handles_multibyte() {
        let mut s = RenameState::new(1, "aé".to_string());
        s.backspace();
        assert_eq!(s.buffer, "a");
        assert_eq!(s.cursor, 1);
    }
}
