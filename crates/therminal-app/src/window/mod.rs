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
mod focus_hint;
mod folder_open;
pub(crate) mod git_ref_open;
mod help_overlay;
mod init;
mod keybindings;
mod launcher_overlay;
mod mouse;
mod pane_ops;
mod reconcile;
mod render;
mod render_driver;
#[cfg(test)]
mod render_tests;
mod settings_overlay;
pub(crate) mod toast;
pub(crate) mod trust_escalation_overlay;
pub(crate) mod wsl_paths;

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

/// Bidirectional mapping between the GUI's local `PaneId` space and the
/// daemon's separately-counted `PaneId` space.
///
/// In remote attach mode the GUI still allocates its own monotonic local
/// ids (via `pane::spawn::next_pane_id`) so the layout tree, focus state,
/// and event-loop addressing stay independent of daemon state. The daemon
/// independently assigns its own ids when sessions/panes are created.
/// This map is the single source of truth for translating between the two
/// spaces; every IPC call that takes a `pane_id` must look up the daemon
/// id here, and every inbound daemon event must be resolved back to a
/// local id before touching GUI state. Local-mode panes never touch this
/// map.
#[derive(Default)]
pub(crate) struct PaneIdMap {
    local_to_daemon: HashMap<PaneId, therminal_protocol::PaneId>,
    daemon_to_local: HashMap<therminal_protocol::PaneId, PaneId>,
}

impl PaneIdMap {
    pub(crate) fn insert(&mut self, local: PaneId, daemon: therminal_protocol::PaneId) {
        self.local_to_daemon.insert(local, daemon);
        self.daemon_to_local.insert(daemon, local);
    }

    #[allow(dead_code)]
    pub(crate) fn daemon_for_local(&self, local: PaneId) -> Option<therminal_protocol::PaneId> {
        self.local_to_daemon.get(&local).copied()
    }

    #[allow(dead_code)]
    pub(crate) fn local_for_daemon(&self, daemon: therminal_protocol::PaneId) -> Option<PaneId> {
        self.daemon_to_local.get(&daemon).copied()
    }

    pub(crate) fn remove_by_local(&mut self, local: PaneId) {
        if let Some(daemon) = self.local_to_daemon.remove(&local) {
            self.daemon_to_local.remove(&daemon);
        }
    }

    /// Return any daemon pane id from the map, or `None` if the map is
    /// empty. Used by tn-fi1k cross-workspace spawn paths to pick an
    /// anchor pane for `IpcRequest::SplitPane` when the caller has no
    /// natural source pane (e.g. switching to a fresh workspace).
    #[allow(dead_code)]
    pub(crate) fn any_daemon_id(&self) -> Option<therminal_protocol::PaneId> {
        self.local_to_daemon.values().next().copied()
    }

    /// All daemon pane IDs currently mapped. Used by workspace
    /// reconciliation (tn-9jhx) to diff against the daemon's authoritative
    /// pane set.
    pub(crate) fn all_daemon_ids(&self) -> Vec<therminal_protocol::PaneId> {
        self.daemon_to_local.keys().copied().collect()
    }
}
use therminal_core::config::{KeyAction, TherminalConfig};
use therminal_core::config_watcher::ConfigWatcher;
use therminal_core::font::PLATFORM_MONOSPACE;
use therminal_core::geometry::Rect;

use keybindings::{BindingLookup, build_binding_map};

// ── Custom event for waking the event loop from the PTY reader ───────────

/// Events sent from background threads to the winit event loop.
#[derive(Debug)]
pub(crate) enum UserEvent {
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
    /// A daemon `SplitPane` RPC fired asynchronously has completed.
    /// Carries everything needed to finish the local layout insert on the
    /// main thread without ever blocking the event loop.
    DaemonSplitComplete(crate::window::pane_ops::DaemonSplitResult),
    /// The daemon broadcast a `WorkspaceChanged` event for our session
    /// (tn-9jhx). An external mutator (MCP tool, CLI command) changed the
    /// workspace topology. The main thread should query `GetWorkspaces`
    /// and reconcile the local layout.
    DaemonWorkspaceChanged {
        session_id: therminal_protocol::SessionId,
    },
    /// Async `build_remote_pane_state` completed for a pane discovered
    /// during workspace reconciliation (tn-9jhx). Mount the pane into the
    /// local layout tree and re-render.
    DaemonReconcilePanesReady(Box<ReconcileResult>),
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

/// Result of the async reconciliation pass (tn-9jhx).
///
/// Built on the tokio runtime after a `DaemonWorkspaceChanged` event.
/// Contains the daemon's authoritative workspace state and newly-built
/// `PaneState` objects for daemon panes the GUI didn't know about.
/// Delivered to the main thread via `UserEvent::DaemonReconcilePanesReady`.
pub(crate) struct ReconcileResult {
    /// Workspace topology from the daemon (authoritative).
    workspaces: Vec<therminal_protocol::daemon::WorkspaceInfo>,
    /// Which workspace the daemon considers active.
    active_workspace: therminal_protocol::WorkspaceId,
    /// Freshly built `PaneState`s for daemon panes that the GUI didn't
    /// already have locally. Each entry is `(local_id, daemon_pane_id, state)`.
    new_panes: Vec<(
        crate::pane::PaneId,
        therminal_protocol::PaneId,
        crate::pane::PaneState,
    )>,
    /// Daemon pane IDs that disappeared (present locally but absent in the
    /// daemon's workspace list). The main thread should close these without
    /// issuing a `KillPane` RPC (the daemon already removed them).
    removed_daemon_ids: Vec<therminal_protocol::PaneId>,
}

impl std::fmt::Debug for ReconcileResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReconcileResult")
            .field("workspaces", &self.workspaces.len())
            .field("new_panes", &self.new_panes.len())
            .field("removed_daemon_ids", &self.removed_daemon_ids)
            .finish()
    }
}

/// Active top-level overlay mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum OverlayMode {
    Help,
    Settings,
    TrustEscalation,
    Launcher,
}

// ── Platform-aware home directory ────────────────────────────────────────

/// Return the user's home directory as a `String`, preferring `$HOME`
/// (which is always correct on Unix and WSL2) and falling back to
/// `dirs::home_dir()` on native Windows where `HOME` is often unset.
///
/// This replaces raw `std::env::var("HOME")` in app-side path helpers
/// (status bar `~` abbreviation, tilde expansion for editor/folder-open
/// hotspots) so that native Windows builds resolve the user profile
/// directory even when no Unix-style `HOME` variable is present.
pub(crate) fn platform_home_dir() -> Option<String> {
    // Fast path: $HOME is set — covers Linux, macOS, and WSL2.
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return Some(home);
    }
    // Fallback: dirs::home_dir() queries the OS profile directory.
    // On Windows this returns {FOLDERID_Profile} (e.g. C:\Users\<user>)
    // without requiring $HOME. On Unix it falls back to getpwuid.
    dirs::home_dir().and_then(|p| p.to_str().map(String::from))
}

// ── GPU state ────────────────────────────────────────────────────────────

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

/// Stashed context for a deferred remote pane spawn (tn-ou30).
///
/// The remote fresh-spawn path in `init_gpu` now defers the actual
/// `spawn_remote_pane` call until the first authoritative window size
/// lands, just like the local path. This struct holds the arguments that
/// were previously passed eagerly.
pub(crate) struct DeferredRemoteSpawn {
    pub(crate) local_id: PaneId,
    pub(crate) daemon_client: Arc<therminal_daemon_client::DaemonClient>,
    pub(crate) tokio_handle: tokio::runtime::Handle,
    pub(crate) daemon_socket: std::path::PathBuf,
    pub(crate) callbacks: crate::pane::PaneCallbacks,
    pub(crate) scrollback: usize,
    pub(crate) interceptor_cfg: therminal_terminal::interceptor::InterceptorConfig,
    pub(crate) reuse_session_id: Option<therminal_protocol::SessionId>,
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

    /// Claude Code session cwd tracker (tn-ykxb). Background thread
    /// polls `/tmp/claude-code-state/*.json` and exposes a pid->cwd
    /// lookup consumed by the renderer to resolve Claude tool-call
    /// hotspots against the agent's working directory.
    pub(crate) claude_cwd: Arc<crate::claude_cwd::ClaudeCwdTracker>,

    /// Proxy to wake the event loop from PTY reader threads.
    event_proxy: EventLoopProxy<UserEvent>,

    /// Current modifiers state from winit.
    modifiers: Modifiers,

    /// Trailing-edge resize debounce.
    pending_resize: Option<PhysicalSize<u32>>,
    last_resize_at: Option<Instant>,

    /// Current cursor position in physical pixels.
    cursor_position: Option<(f64, f64)>,

    /// Whether the cursor was in the CSD header area on the last motion event.
    /// Used to trigger a redraw when the mouse exits the header, clearing hover.
    cursor_was_in_csd_header: bool,

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

    /// Pixel position of the last left-button press (for click jitter tolerance).
    last_press_pixel: Option<(f64, f64)>,

    /// Click count (1 = single, 2 = double/word, 3 = triple/line).
    click_count: u8,

    /// Current loaded configuration.
    config: TherminalConfig,

    /// Cached subset of `config.hotspots.git_tools` whose binaries were
    /// found on `PATH` at config-load time (tn-fzr0). Recomputed in
    /// `apply_config` when the git_tools list changes. Drives the git
    /// commit hash hotspot context menu — only tools in this set get a
    /// menu entry, and they appear in `git_tools` order. Empty when no
    /// tool is installed; the menu then falls back to "Copy hash" only.
    pub(crate) discovered_git_tools: Vec<String>,

    /// Parsed keybinding lookup map (rebuilt on config reload).
    binding_map: HashMap<BindingLookup, KeyAction>,

    /// Config file watcher handle (kept alive).
    _config_watcher: Option<ConfigWatcher>,

    /// Last split direction used (for auto-direction alternation).
    last_split_direction: SplitDirection,

    /// Active overlay mode, if any.
    overlay_mode: Option<OverlayMode>,

    /// Scroll offset (in row units) for the keybinding help overlay body.
    help_overlay_scroll_rows: u32,

    /// Settings overlay registry + keyboard navigation state.
    settings_overlay: settings_overlay::SettingsOverlayState,

    /// Launcher overlay state (profile tile grid, tn-47ix).
    launcher_state: launcher_overlay::LauncherState,

    /// Pending trust escalation modal state (tn-b99).
    trust_escalation: Option<trust_escalation_overlay::TrustEscalationState>,

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

    /// Sender into the swarm debouncer channel (tn-s8w3). Cloned into each
    /// remote pane forwarder so hook-driven `SubagentStarted`/`SubagentStopped`
    /// daemon events can feed the debouncer without file scanning.
    pub(crate) swarm_debouncer_tx:
        Option<std::sync::mpsc::Sender<crate::pane::swarm_watcher::SwarmWatcherEvent>>,

    /// Shared set of Claude session IDs running in this instance's panes,
    /// read by the swarm watcher thread when
    /// `general.swarm_watch_scope = "current"` to restrict subagents to
    /// those whose parent Claude Code session belongs to this instance.
    /// Populated from `PaneStatus.claude_session_id` each tick (tn-twfg).
    pub(crate) swarm_pane_session_ids: Option<crate::pane::swarm_watcher::PaneSessionIdProvider>,

    /// Map of subagent agent_id -> pane_id for panes spawned by the swarm
    /// watcher, so reclaim events can find the right pane to close.
    pub(crate) swarm_panes: HashMap<String, PaneId>,

    /// Cursor blink visibility state (tn-ya01). Toggled every ~530ms when
    /// `config.cursor.blink` is true and `accessibility.reduced_motion` is
    /// false. When false, the cursor rect is skipped during rendering.
    pub(crate) cursor_blink_visible: bool,

    /// Timestamp of the last cursor blink toggle (tn-ya01).
    pub(crate) last_cursor_blink: Instant,

    /// Timestamp when a visual bell flash started (for timed invert effect).
    visual_bell_start: Option<Instant>,

    /// Pre-zoom layout tree, stored when a pane is zoomed to fullscreen.
    /// Contains the full layout with the zoomed pane replaced by `Empty`.
    zoomed_layout: Option<LayoutNode>,

    /// Hit-test areas captured from the most recent status bar render.
    /// Used by the mouse handler to detect clicks on chrome elements like
    /// the agent indicator.
    status_bar_hit_areas: chrome::StatusBarHitAreas,

    /// Delegate sibling summary state machine (tn-ztv3.4). Tracks per-pane
    /// state for `/gg-delegate` siblings so the status bar can render a
    /// compact `delegates: planner=streaming (87s), reviewer=idle` section
    /// without forcing the user to open each pane. Always-on: appears
    /// automatically when delegate-tagged panes exist and fades out ~5 s
    /// after every sibling reaches a terminal state.
    pub(crate) delegate_summary: chrome::DelegateSummaryState,

    /// Active transient toast notification (lower-right), if any. Set by
    /// `show_toast`, cleared when expired. Used by `open_in_editor` to
    /// surface failures visibly and by the semantic region jump handler.
    pub(crate) toast: Option<toast::Toast>,

    /// Persistent connection to the therminal-daemon. Established at startup
    /// in `main()` before window creation. Currently kept alive but not yet
    /// used to drive panes — that wiring lands in tn-382v follow-ups. The
    /// `Arc` lets future subsystems (PaneBackend, MCP forwarder, agent
    /// observers) clone a handle without re-connecting.
    #[allow(dead_code)]
    pub(crate) daemon_client: Option<Arc<therminal_daemon_client::DaemonClient>>,

    /// Handle to the leaked tokio runtime that hosts the `DaemonClient`.
    /// Used by `init_gpu` (tn-ytw2) to drive attach-flow RPCs from the
    /// winit event-loop thread, which has no ambient tokio context — so
    /// `Handle::try_current()` returns None there. This stored handle is
    /// always valid because the runtime is intentionally leaked in `main`.
    pub(crate) daemon_runtime: Option<tokio::runtime::Handle>,

    /// Local↔daemon `PaneId` mapping for remote-mode panes (tn-pgz6).
    /// Empty in pure local mode.
    pub(crate) pane_id_map: PaneIdMap,

    /// Daemon `SessionId` this GUI is publishing workspace state under.
    /// `None` in local mode or before the remote attach completes. Set by
    /// the remote-spawn / attach path (tn-pgz6, tn-ytw2). When `None` the
    /// `publish_workspace_state()` helper short-circuits.
    pub(crate) daemon_session_id: Option<therminal_protocol::SessionId>,

    /// Shared system metrics snapshot (tn-l6y3). Updated by a background
    /// poller thread; read each frame to populate the status bar right
    /// section with CPU/memory stats.
    pub(crate) system_metrics: Option<crate::system_metrics::SharedMetrics>,

    /// App-side read-only pattern engine (tn-f9cl).
    pub(crate) pattern_engine: Option<therminal_terminal::semantic_patterns::PatternEngine>,

    /// Pre-rasterized overlay widgets (tn-npd). Created lazily during
    /// `init_gpu` because the render pipeline needs the surface format
    /// and device. `None` until the GPU is up; widgets are skipped
    /// silently on frames before then.
    pub(crate) widget_renderer: Option<crate::widgets::WidgetRenderer>,

    /// Freshness cache for rasterized widget textures (tn-npd). Owns
    /// the `WidgetRasterizer` and the `HashMap<WidgetId, CachedWidget>`
    /// of uploaded textures. Safe to initialize eagerly since it holds
    /// no GPU handles until the first `upsert`.
    pub(crate) widget_manager: crate::widgets::WidgetManager,

    /// Agent timeline overlay widget (tn-x85k). Maintains a ring buffer
    /// of tool entries and rasterizes a colored bar on demand.
    pub(crate) agent_timeline: crate::widgets::agent_timeline::AgentTimelineSource,

    /// tn-ou30: deferred initial pane spawn (local or remote).
    ///
    /// On Windows native builds the size reported by `Window::inner_size()`
    /// immediately after `create_window()` does not always match the size
    /// the OS settles on once the window is shown — DPI snapping, taskbar
    /// reservation, and DWM reshape can all change the surface dimensions
    /// before the first frame. The fresh shell would then emit its first
    /// prompt against a stale row count, which alacritty's resize cannot
    /// retroactively fix (the prompt has already landed at the wrong row).
    ///
    /// To avoid this, both local and remote fresh-spawn branches in
    /// `init_gpu` defer the actual spawn. They store window/gpu/grid_renderer
    /// and set this flag to `true`. The first authoritative size — either
    /// the first `WindowEvent::Resized` or, as a fallback, the first
    /// `RedrawRequested` — calls `ensure_initial_pane_spawned()`,
    /// which builds the pane against the current GPU surface dims and
    /// clears the flag.
    pub(crate) initial_pane_pending: bool,

    /// Runtime-only "focus mode" toggle (tn-t2yd.2). When true, all chrome
    /// (pane headers, status bar, tab bar) is hidden so the terminal grid
    /// owns the whole window. Bound to `KeyAction::FocusMode` (F11 by
    /// default). Not persisted — this is a transient user choice, not a
    /// preference.
    pub(crate) focus_mode: bool,

    /// tn-sfn9: hover-reveal hint visible at top edge when focus mode is
    /// active. Set to `true` when the mouse Y is within 4 px of the top
    /// edge, `false` otherwise.
    pub(crate) focus_mode_hint_visible: bool,

    /// tn-rl6i: tracks whether the window is currently minimized. On Windows,
    /// minimizing sends a Resized(0, 0) event. When restoring, the subsequent
    /// non-zero Resized event must trigger a full relayout so the grid renders
    /// at the correct dimensions.
    pub(crate) minimized: bool,

    /// Stashed state for a deferred remote fresh-spawn. `None` when the
    /// deferred spawn is local-mode or when no spawn is pending.
    pub(crate) deferred_remote_spawn: Option<DeferredRemoteSpawn>,

    /// tn-ou30: countdown (in redraw frames) until the initial scrollback
    /// compaction fires. After the initial pane spawn, the shell may emit
    /// a leading newline or other startup output that creates a spurious
    /// scrollback row before the first prompt. A synthetic resize-down-
    /// then-up "trick" compacts that row away. We wait a few frames so
    /// the shell's first output has landed in the Term.
    pub(crate) scrollback_compact_countdown: u8,
}

/// Rewrite a `LayoutSnapshot` tree, translating every leaf pane id from
/// the GUI's local id space into the daemon's id space via `map`.
/// Returns `None` if any leaf has no mapping (caller should drop the
/// layout and fall back to a flat cascade on attach).
fn translate_layout_snapshot(
    snap: &therminal_protocol::daemon::LayoutSnapshot,
    map: &PaneIdMap,
) -> Option<therminal_protocol::daemon::LayoutSnapshot> {
    use therminal_protocol::daemon::LayoutSnapshot;
    match snap {
        LayoutSnapshot::Leaf { pane_id } => {
            // The snapshot's "pane_id" here is a local GUI PaneId that
            // workspace_info() copied verbatim from the layout tree.
            let local = *pane_id as crate::pane::PaneId;
            let daemon = map.daemon_for_local(local)?;
            Some(LayoutSnapshot::Leaf { pane_id: daemon })
        }
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => Some(LayoutSnapshot::Split {
            direction: *direction,
            ratio: *ratio,
            first: Box::new(translate_layout_snapshot(first, map)?),
            second: Box::new(translate_layout_snapshot(second, map)?),
        }),
    }
}

impl App {
    /// Show a toast notification in the lower-right corner. Replaces any
    /// existing toast and schedules a redraw so it becomes visible
    /// immediately.
    ///
    /// Lifetime is [`toast::TOAST_TTL`] (2.5s). Use for short user-facing
    /// failure messages that would otherwise be silently swallowed by
    /// `tracing::warn!`.
    pub(crate) fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some(toast::Toast::new(text, Instant::now(), toast::TOAST_TTL));
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

/// Compute the display labels for the workspace tab bar.
///
/// Free function (not a method) so callers that already hold a mutable
/// borrow on `App.grid_renderer` can still build labels without conflict.
///
/// Precedence per workspace:
/// 1. Inline rename in progress: show buffer + trailing cursor.
/// 2. Custom name set via rename: show `"<id>: <name>"`.
/// 3. Focused pane has a Claude session title (tn-5fgz / tn-lxq9): show
///    `"<id>: <title>"`, truncated to [`MAX_TAB_LABEL_CHARS`] with
///    [`chrome::TAB_ELLIPSIS`]. The `"<id>: "` prefix is always preserved.
/// 4. Focused pane has a known cwd: show `"<id>: <basename>"`.
/// 5. Otherwise: just `"<id>"`.
pub(crate) fn build_tab_labels(
    workspace_ids: &[usize],
    workspaces: Option<&WorkspaceManager>,
    rename_state: Option<&RenameState>,
    claude_titles: Option<&std::collections::HashMap<usize, String>>,
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
            // tn-5fgz / tn-lxq9: Use Claude session title when available.
            // Truncate to MAX_TAB_LABEL_CHARS so long prompts don't blow up
            // the tab slot; char-count (not byte-slice) to stay unicode-safe.
            if let Some(title) = claude_titles
                .and_then(|ct| ct.get(&ws_id))
                .filter(|t| !t.is_empty())
            {
                return truncate_tab_label(ws_id, title);
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

/// Maximum char count for a rendered tab label when composed from a
/// Claude session title. The chrome tab-bar renderer (`tab_bar.rs`)
/// applies its own shape-and-measure fit pass for rendering, but we also
/// cap at the logical level here so the label that feeds hit-tests and
/// tests doesn't grow unbounded.
pub(crate) const MAX_TAB_LABEL_CHARS: usize = 24;

/// Compose `"<id>: <title>"` and truncate the title portion with the
/// chrome ellipsis glyph so the total stays ≤ [`MAX_TAB_LABEL_CHARS`].
///
/// The `"<id>: "` prefix is always preserved. If the prefix alone is
/// already longer than the budget (unreachable in practice — workspace
/// IDs are small integers), we return the full untruncated label
/// unchanged rather than producing something meaningless.
fn truncate_tab_label(ws_id: usize, title: &str) -> String {
    let prefix = format!("{ws_id}: ");
    let prefix_chars = prefix.chars().count();
    let full = format!("{prefix}{title}");
    let full_chars = full.chars().count();

    if full_chars <= MAX_TAB_LABEL_CHARS {
        return full;
    }

    // Leave room for at least the prefix + one ellipsis. If the prefix
    // alone already meets or exceeds the budget, skip truncation — this
    // isn't this function's job to fix (see doc comment above).
    if prefix_chars + 1 >= MAX_TAB_LABEL_CHARS {
        return full;
    }

    let title_budget = MAX_TAB_LABEL_CHARS - prefix_chars - 1; // -1 for ellipsis
    let truncated_title: String = title.chars().take(title_budget).collect();
    let mut out = prefix;
    out.push_str(&truncated_title);
    out.push(chrome::TAB_ELLIPSIS);
    out
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

        // ── Git TUI tools rediscovery (tn-fzr0) ────────────────────────
        if self.config.hotspots.git_tools != old_config.hotspots.git_tools {
            self.discovered_git_tools =
                git_ref_open::discover_git_tools(&self.config.hotspots.git_tools);
            info!(
                count = self.discovered_git_tools.len(),
                tools = ?self.discovered_git_tools,
                "git TUI tools rediscovered on config reload"
            );
        }

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

        // UI font family change only needs an overlay cache clear, not a full
        // cell-metrics rebuild, so handle it separately from grid font changes.
        if self.config.font.ui_font_family != old_config.font.ui_font_family
            && let Some(renderer) = self.grid_renderer.as_mut()
        {
            renderer.font_config.ui_font_family = self.config.font.ui_font_family.clone();
            renderer.clear_render_caches();
            info!(
                ui_font_family = %self.config.font.ui_font_family,
                "UI font family updated via hot-reload"
            );
        }

        // ── Padding hot-reload ───────────────────────────────────────────
        let padding_changed =
            (self.config.general.padding - old_config.general.padding).abs() > f32::EPSILON;

        // ── Color overrides hot-reload ──────────────────────────────────
        // Always re-apply on any config change. The prior field-by-field
        // diff only examined {background, foreground, cursor, selection,
        // ansi} and missed every chrome/hotspot role override added by
        // tn-g7oo (chrome_focus_border, chrome_header_bg, chrome_status_bar_bg,
        // chrome_fg*, hotspot_*, chrome_hyperlink, ...). Editing any of those
        // silently failed to re-skin the chrome (status bar, tab bar, CSD
        // strip, pane headers) until the next non-color change or restart.
        // `apply_color_overrides` is cheap (field assigns + hex parses +
        // clear_render_caches) and `apply_config` is already debounced at
        // 500ms by the config watcher, so unconditionally running it on
        // every config event is the robust fix.
        if let Some(renderer) = self.grid_renderer.as_mut() {
            renderer.apply_color_overrides_with_contrast(
                &self.config.colors,
                self.config.accessibility.high_contrast,
            );
            info!("color overrides re-applied via hot-reload");
        }

        // Apply accessibility ui_text_scale on every config change (tn-avjv.6).
        if let Some(renderer) = self.grid_renderer.as_mut() {
            renderer.set_ui_text_scale(self.config.accessibility.ui_text_scale);
        }

        let needs_relayout = font_changed || padding_changed;

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
                new_font_config.ui_font_family = self.config.font.ui_font_family.clone();
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
            let chrome_visible = !self.focus_mode;
            let workspace_count = self.workspaces.as_ref().map(|wm| wm.len()).unwrap_or(1);
            let full_rect = crate::pane::content_area_rect_csd(
                gpu.config.width as f32,
                gpu.config.height as f32,
                chrome_visible,
                workspace_count,
                self.config.general.use_csd,
                self.focus_mode,
            );
            if let Some(wm) = self.workspaces.as_mut() {
                let layout = wm.layout_mut();
                layout.layout(full_rect);
                layout.resize_all_panes(renderer, chrome_visible);
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

        // ── Agent timeline hot-reload ────────────────────────────────────
        {
            let new_tc = &self.config.widgets.agent_timeline;
            let old_tc = &old_config.widgets.agent_timeline;
            if new_tc.height_px != old_tc.height_px
                || new_tc.max_entries != old_tc.max_entries
                || new_tc.position != old_tc.position
            {
                self.agent_timeline.update_config(
                    new_tc.max_entries,
                    new_tc.height_px,
                    new_tc.position,
                );
                info!("agent timeline config updated via hot-reload");
            }
            // Wire the `enabled` config field to runtime visibility on
            // hot-reload. The keybinding toggle works independently, but
            // when the user edits `enabled` in the TOML we respect it.
            if new_tc.enabled != old_tc.enabled {
                self.agent_timeline.visible = new_tc.enabled;
                if !new_tc.enabled {
                    self.widget_manager
                        .remove(crate::widgets::TIMELINE_WIDGET_ID);
                }
                info!(
                    enabled = new_tc.enabled,
                    "agent timeline enabled changed via hot-reload"
                );
            }
        }

        let patterns_changed = self.config.patterns != old_config.patterns;
        if patterns_changed {
            if self.config.patterns.enabled {
                use therminal_terminal::semantic_patterns::{PatternEngine, PatternEngineConfig};
                self.pattern_engine = Some(PatternEngine::new(PatternEngineConfig {
                    enabled: true,
                    user_pattern_dir: self.config.patterns.directory.clone(),
                    shipped_pattern_dir: None,
                    max_patterns: self.config.patterns.max_patterns,
                    slow_pattern_threshold_us: self.config.patterns.slow_pattern_threshold_us,
                    slow_strike_limit: self.config.patterns.slow_strike_limit,
                }));
                info!("pattern engine re-instantiated via hot-reload");
            } else {
                self.pattern_engine = None;
                info!("pattern engine disabled via hot-reload");
            }
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
        // tn-beez Phase B: mirror focus to the daemon so MCP
        // `terminal.panes.list` focused_pane matches the GUI.
        if let Some(id) = id {
            self.select_pane_remote(id);
        }
    }

    /// Get the focused pane's OSC 7 cwd, if any (tn-vm2j).
    ///
    /// Used by the hotspot click handlers to resolve shell-relative
    /// paths against the pane's shell cwd instead of therminal's
    /// process cwd. Returns an owned `String` because the borrow on
    /// the workspace layout can't outlive the method call — the
    /// click handlers then call `&mut self` methods on `App`, and
    /// the underlying cwd lives behind a `Mutex` inside `PaneStatus`
    /// that must be unlocked before those calls.
    pub(crate) fn focused_pane_cwd(&self) -> Option<String> {
        let id = self.focused_pane()?;
        let layout = self.get_layout()?;
        let pane = layout.find_pane(id)?;
        let status = pane.status.lock().ok()?;
        status.cwd.clone()
    }

    /// Whether chrome (pane headers, status bar, tab bar) should be drawn.
    ///
    /// tn-t2yd.2: returns `false` when focus mode is active, baking the
    /// default choice (chrome on) into the source. Individual chrome pieces
    /// used to be flag-gated via `[general] show_status_bar` /
    /// `show_pane_headers` but those config fields were removed — the
    /// tripwire now is a single F11 runtime toggle.
    pub(crate) fn chrome_visible(&self) -> bool {
        !self.focus_mode
    }

    /// Compute the content area rect from GPU dimensions.
    /// Returns `None` if the GPU state is not yet initialized.
    ///
    /// Respects `focus_mode` (tn-t2yd.2): when active, the status bar is
    /// collapsed and the tab bar is collapsed unless CSD is in use (since
    /// CSD owns the top strip for window controls, which must remain
    /// reachable so the user can still close/move the window).
    pub(crate) fn compute_layout_rect(&self) -> Option<Rect> {
        let gpu = self.gpu.as_ref()?;
        let workspace_count = self.workspaces.as_ref().map(|wm| wm.len()).unwrap_or(1);
        Some(crate::pane::content_area_rect_csd(
            gpu.config.width as f32,
            gpu.config.height as f32,
            self.chrome_visible(),
            workspace_count,
            self.config.general.use_csd,
            self.focus_mode,
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
        let show_headers = self.chrome_visible();
        if let Some(layout) = self.get_layout_mut() {
            layout.layout(full_rect);
        }
        // Separate borrow scope: layout_mut + renderer.
        if let (Some(wm), Some(renderer)) = (self.workspaces.as_mut(), self.grid_renderer.as_ref())
        {
            wm.layout_mut().resize_all_panes(renderer, show_headers);
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Publish the current workspace topology to the daemon (tn-k3yo).
    ///
    /// Called after every GUI-side topology mutation (split, close, swap,
    /// workspace switch/create/rename, send-pane-to-workspace) so the
    /// daemon's stored `WorkspaceInfo` snapshots stay in sync with what
    /// the user sees. Required by the attach path (tn-ytw2) and by MCP
    /// `terminal.workspaces.list` queries.
    ///
    /// Behaviour:
    /// - No daemon client → no-op (local mode pays nothing).
    /// - No `daemon_session_id` yet → debug log + no-op (pre-attach).
    /// - Wrapped in a 500ms `tokio::time::timeout`; failures are logged
    ///   at warn level and swallowed. This MUST never block the UI thread.
    ///
    /// Note: in remote mode the `WorkspaceInfo.pane_ids` should be daemon
    /// `PaneId`s. We translate via `pane_id_map.daemon_for_local()`. Any
    /// local id without a mapping is dropped from the published list and
    /// a debug line is emitted — this should only happen transiently
    /// during pane setup.
    /// Publish the current workspace state to the daemon. Returns `true`
    /// if the publish was actually issued (or short-circuited because there
    /// is no daemon — i.e. local-only mode), and `false` if the translation
    /// guard fired and the publish was silently skipped due to mixed
    /// local/remote pane ids. F12 (tn-97j6): callers performing user-visible
    /// state mutations (e.g. workspace rename) check the return value so
    /// they can warn the user when their change didn't reach the daemon.
    pub(crate) fn publish_workspace_state(&self) -> bool {
        let Some(client) = self.daemon_client.as_ref() else {
            return true;
        };
        let Some(session_id) = self.daemon_session_id else {
            debug!("publish_workspace_state: no daemon_session_id yet; skipping (pre-attach)");
            return true;
        };
        let Some(wm) = self.workspaces.as_ref() else {
            return true;
        };

        // Translate local pane ids → daemon pane ids in the snapshot.
        //
        // Guard: if ANY local id in any workspace can't be translated, the
        // GUI is in a mixed state (some panes came from the daemon via
        // attach/remote_spawn, others were created by local-mode split/close
        // paths that haven't been cut over to daemon IPC yet — that's
        // tn-beez / Phase B). Publishing a partial layout would corrupt the
        // daemon's stored workspace_state and cause the next attach to
        // reconstruct garbage. In that case, skip the publish entirely and
        // log a debug line. This is a defensive no-op once Phase B lands
        // and every pane has a daemon id.
        //
        // Also: workspace_info() builds `layout: Some(LayoutSnapshot)` from
        // the local layout tree, so leaves contain LOCAL pane ids. We have
        // to walk the snapshot and rewrite each leaf's pane_id through the
        // map as well — or drop the layout if any leaf doesn't translate.
        let mut workspaces_info = wm.workspace_info();
        let mut translation_failed = false;
        for ws in workspaces_info.iter_mut() {
            let translated: Vec<_> = ws
                .pane_ids
                .iter()
                .map(|local| self.pane_id_map.daemon_for_local(*local))
                .collect();
            if translated.iter().any(|t| t.is_none()) {
                translation_failed = true;
                break;
            }
            ws.pane_ids = translated.into_iter().flatten().collect();
            ws.focused_pane = ws
                .focused_pane
                .and_then(|local| self.pane_id_map.daemon_for_local(local));
            // Rewrite layout snapshot leaves from local → daemon ids.
            // If any leaf fails, drop the layout entirely (attach path
            // falls back to flat cascade via from_flat_pane_ids).
            if let Some(layout) = ws.layout.as_ref() {
                match translate_layout_snapshot(layout, &self.pane_id_map) {
                    Some(translated) => ws.layout = Some(translated),
                    None => {
                        translation_failed = true;
                        break;
                    }
                }
            }
        }
        if translation_failed {
            // tn-fi1k: Phase B finally cut switch_workspace / send_to_workspace
            // / restore_layout over to daemon RPCs, so reaching this branch
            // is a bug — every pane in any workspace should now have a
            // daemon id. Keep the safety net (don't publish a partial
            // layout) but log loud so a regression is visible. The first
            // hit per process is a `warn!`; subsequent hits stay debug-only
            // to avoid log spam if a bug repeats every redraw.
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "publish_workspace_state: tn-5hqp guard fired — at least one pane has no daemon id mapping despite tn-fi1k cutover. This indicates a regression: a code path is creating panes via crate::pane::spawn_pane in daemon mode without populating pane_id_map."
                );
            } else {
                debug!(
                    "publish_workspace_state: tn-5hqp guard fired again (warn-once already issued)"
                );
            }
            return false;
        }
        let active_workspace = wm.active_id() as therminal_protocol::WorkspaceId;

        let request = therminal_protocol::daemon::IpcRequest::SetWorkspaceState {
            session_id,
            workspaces: workspaces_info,
            active_workspace,
        };

        // Use the stored daemon runtime handle (see main.rs::connect_daemon).
        // `Handle::try_current()` is None on the winit event-loop thread.
        // Fire-and-forget: `SetWorkspaceState` is advisory (layout persistence
        // for MCP / CLI callers). Blocking the event loop here stalls all
        // window repaints for the duration of the RPC — visible as glitches
        // in other windows. The daemon applies the update asynchronously and
        // the GUI never reads the response.
        let Some(handle) = self.daemon_runtime.clone() else {
            tracing::warn!("publish_workspace_state: no daemon runtime handle");
            return true;
        };
        let client = Arc::clone(client);
        handle.spawn(async move {
            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                client.send_request(request),
            )
            .await
            {
                Ok(Ok(_)) => {
                    debug!(session_id, "published workspace state to daemon");
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "publish_workspace_state: daemon request failed");
                }
                Err(_) => {
                    tracing::warn!("publish_workspace_state: daemon request timed out (>500ms)");
                }
            }
        });
        true
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
                // reduced_motion suppresses the visual bell animation (tn-avjv.6).
                if !self.config.accessibility.reduced_motion {
                    self.visual_bell_start = Some(Instant::now());
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
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
            .with_window_icon(load_window_icon())
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
            UserEvent::DaemonSplitComplete(result) => {
                self.finish_split_pane_remote(result);
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
            UserEvent::DaemonWorkspaceChanged { session_id } => {
                info!(session_id, "received DaemonWorkspaceChanged event");
                self.handle_daemon_workspace_changed(session_id);
            }
            UserEvent::DaemonReconcilePanesReady(result) => {
                info!(?result, "applying reconciliation result");
                self.apply_reconcile_result(*result);
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
            WindowEvent::Occluded(false) => {
                // tn-rl6i: belt-and-suspenders for restore from minimize.
                // Some compositors fire Occluded(false) without a
                // corresponding Resized event on un-minimize. Force a
                // relayout so the grid dimensions are correct.
                if self.minimized {
                    info!("window un-occluded while minimized, forcing relayout");
                    self.minimized = false;
                    self.relayout_and_redraw();
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
                self.handle_keyboard_input_event(key_event);
            }
            _ => {}
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────

/// Create the event loop, set control flow to Wait, and run the app.
///
/// # Daemon runtime invariant
///
/// F14 (tn-97j6): `daemon_runtime` MUST be a `Handle` to a tokio runtime
/// whose lifetime exceeds the entire winit event loop — typically a
/// runtime intentionally `Box::leak`ed (or `Arc`-held) by `main.rs`. The
/// GUI's winit thread has no ambient tokio context, so every daemon RPC
/// reuses this handle via `block_on` or `spawn`. If the runtime backing
/// the handle is dropped while the event loop is still running, every
/// subsequent daemon RPC panics. The current `main.rs::connect_daemon`
/// satisfies this by leaking the runtime; any future caller of `run()`
/// MUST do the same. (A type-enforcing fix — pass an
/// `Arc<tokio::runtime::Runtime>` and store it alongside the `Handle` —
/// is tracked as out-of-scope for tn-97j6.)
pub fn run(
    daemon_client: Option<Arc<therminal_daemon_client::DaemonClient>>,
    daemon_runtime: Option<tokio::runtime::Handle>,
) -> Result<()> {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Therminal panic: {info}");
        eprintln!("Backtrace: {:?}", std::backtrace::Backtrace::capture());
    }));

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy.clone());
    app.daemon_client = daemon_client.clone();
    app.daemon_runtime = daemon_runtime.clone();

    // tn-9jhx: spawn a daemon event listener that forwards workspace
    // topology changes to the winit event loop. Uses a dedicated
    // DaemonClient connection (separate from the GUI's primary client)
    // so Subscribe doesn't contend with request/response traffic.
    if let (Some(client), Some(handle)) = (&daemon_client, &daemon_runtime) {
        let proxy_for_listener = proxy.clone();
        let socket = client.socket_path().to_path_buf();
        handle.spawn(async move {
            if let Err(e) = daemon_event_listener(socket, proxy_for_listener).await {
                warn!("daemon event listener exited: {e}");
            }
        });
    }

    event_loop.run_app(&mut app)?;

    Ok(())
}

/// Daemon event listener task (tn-9jhx).
///
/// Opens a dedicated `DaemonClient` connection and subscribes to
/// `WorkspaceChanged` events. Each event is forwarded to the winit event
/// loop via `EventLoopProxy`. The dedicated connection avoids contention
/// with the GUI's primary client and its per-pane `PaneOutput`
/// subscriptions.
async fn daemon_event_listener(
    socket: std::path::PathBuf,
    proxy: EventLoopProxy<UserEvent>,
) -> anyhow::Result<()> {
    use therminal_daemon_client::DaemonClient;
    use therminal_protocol::daemon::{DaemonEvent, EventKind, IpcResponse};

    let client =
        DaemonClient::connect_with_timeout(&socket, therminal_daemon_client::GUI_REQUEST_TIMEOUT)
            .await?;

    match client
        .subscribe_events(vec![EventKind::WorkspaceChanged])
        .await?
    {
        IpcResponse::Subscribed { .. } => {}
        IpcResponse::Error { message } => {
            anyhow::bail!("subscribe failed: {message}");
        }
        other => {
            anyhow::bail!("unexpected subscribe response: {other:?}");
        }
    }
    info!("daemon event listener subscribed to WorkspaceChanged");

    loop {
        let event = match client.recv_event().await {
            Some(e) => e,
            None => {
                info!("daemon event listener: connection closed");
                break;
            }
        };
        if let DaemonEvent::WorkspaceChanged { session_id, .. } = event
            && proxy
                .send_event(UserEvent::DaemonWorkspaceChanged { session_id })
                .is_err()
        {
            // Event loop closed — exit gracefully.
            break;
        }
    }
    Ok(())
}

/// Load the embedded 32×32 app icon for the window/taskbar.
///
/// On Windows the taskbar icon comes from the HWND's `HICON`, not the exe's
/// embedded resource, so we must set it explicitly via winit.  On X11/Wayland
/// the same mechanism sets the desktop/panel icon.  macOS ignores it (uses
/// the app bundle icon) so returning `None` there is fine.
fn load_window_icon() -> Option<winit::window::Icon> {
    static ICON_PNG: &[u8] = include_bytes!("../../../../resources/therminal-32.png");
    let pixmap = tiny_skia::Pixmap::decode_png(ICON_PNG).ok()?;
    let (w, h) = (pixmap.width(), pixmap.height());
    // tiny-skia stores premultiplied RGBA; winit expects straight RGBA.
    let mut rgba = pixmap.take();
    for pixel in rgba.chunks_exact_mut(4) {
        let a = pixel[3] as f32;
        if a > 0.0 && a < 255.0 {
            let inv = 255.0 / a;
            pixel[0] = (pixel[0] as f32 * inv).min(255.0) as u8;
            pixel[1] = (pixel[1] as f32 * inv).min(255.0) as u8;
            pixel[2] = (pixel[2] as f32 * inv).min(255.0) as u8;
        }
    }
    winit::window::Icon::from_rgba(rgba, w, h).ok()
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
    fn build_tab_labels_rename_branch_includes_buffer_and_cursor() {
        use super::{RenameState, build_tab_labels};
        let mut state = RenameState::new(1, "1".to_string());
        // Initial: label is "1: 1_"
        let labels = build_tab_labels(&[1, 2], None, Some(&state), None);
        assert_eq!(labels[0], "1: 1_");
        assert_eq!(labels[1], "2");
        // After typing 'a','b','c': label is "1: 1abc_"
        for c in ['a', 'b', 'c'] {
            state.insert_char(c);
        }
        let labels = build_tab_labels(&[1, 2], None, Some(&state), None);
        assert_eq!(labels[0], "1: 1abc_");
    }

    #[test]
    fn pane_id_map_insert_and_lookup() {
        use super::PaneIdMap;
        let mut m = PaneIdMap::default();
        m.insert(42, 7);
        assert_eq!(m.daemon_for_local(42), Some(7));
        assert_eq!(m.local_for_daemon(7), Some(42));
        assert_eq!(m.daemon_for_local(99), None);
        assert_eq!(m.local_for_daemon(99), None);
    }

    #[test]
    fn pane_id_map_remove_clears_both_directions() {
        use super::PaneIdMap;
        let mut m = PaneIdMap::default();
        m.insert(42, 7);
        m.remove_by_local(42);
        assert_eq!(m.daemon_for_local(42), None);
        assert_eq!(m.local_for_daemon(7), None);
    }

    #[test]
    fn pane_id_map_any_daemon_id_returns_none_when_empty() {
        use super::PaneIdMap;
        let m = PaneIdMap::default();
        assert_eq!(m.any_daemon_id(), None);
    }

    #[test]
    fn pane_id_map_any_daemon_id_returns_value_when_populated() {
        use super::PaneIdMap;
        let mut m = PaneIdMap::default();
        m.insert(1, 100);
        m.insert(2, 200);
        // Either 100 or 200 is acceptable — order is HashMap-iter-defined.
        let any = m.any_daemon_id().expect("expected Some");
        assert!(any == 100 || any == 200, "got {any}");
    }

    #[test]
    fn backspace_handles_multibyte() {
        let mut s = RenameState::new(1, "aé".to_string());
        s.backspace();
        assert_eq!(s.buffer, "a");
        assert_eq!(s.cursor, 1);
    }

    // ── Claude session title → tab label (tn-lxq9) ─────────────────────

    #[test]
    fn build_tab_labels_uses_claude_title_when_present() {
        use super::build_tab_labels;
        use std::collections::HashMap;
        let mut titles = HashMap::new();
        titles.insert(1usize, "fix login bug".to_string());
        let labels = build_tab_labels(&[1, 2], None, None, Some(&titles));
        assert_eq!(labels[0], "1: fix login bug");
        // Workspace 2 has no title and no cwd → bare id.
        assert_eq!(labels[1], "2");
    }

    #[test]
    fn build_tab_labels_ignores_empty_claude_title() {
        use super::build_tab_labels;
        use std::collections::HashMap;
        let mut titles = HashMap::new();
        titles.insert(1usize, String::new());
        let labels = build_tab_labels(&[1], None, None, Some(&titles));
        // Empty title should fall through to the bare-id branch (no cwd,
        // no rename, no workspace manager) rather than emit "1: ".
        assert_eq!(labels[0], "1");
    }

    #[test]
    fn build_tab_labels_truncates_long_claude_title_with_ellipsis() {
        use super::{MAX_TAB_LABEL_CHARS, build_tab_labels};
        use crate::window::chrome::TAB_ELLIPSIS;
        use std::collections::HashMap;
        let mut titles = HashMap::new();
        // "1: " = 3 chars → title budget = 24 - 3 - 1 = 20 chars of title
        // before the ellipsis.
        let long = "this title is definitely much longer than the budget";
        titles.insert(1usize, long.to_string());
        let labels = build_tab_labels(&[1], None, None, Some(&titles));
        let label = &labels[0];
        assert_eq!(label.chars().count(), MAX_TAB_LABEL_CHARS);
        assert!(label.starts_with("1: "));
        assert!(
            label.ends_with(TAB_ELLIPSIS),
            "expected ellipsis suffix, got {label:?}",
        );
        // Sanity: the first 3 chars of the visible title survived.
        assert!(label.contains("this"));
    }

    #[test]
    fn build_tab_labels_does_not_truncate_short_title() {
        use super::{MAX_TAB_LABEL_CHARS, build_tab_labels};
        use std::collections::HashMap;
        let mut titles = HashMap::new();
        titles.insert(1usize, "short".to_string());
        let labels = build_tab_labels(&[1], None, None, Some(&titles));
        assert_eq!(labels[0], "1: short");
        assert!(labels[0].chars().count() <= MAX_TAB_LABEL_CHARS);
    }

    #[test]
    fn build_tab_labels_unicode_title_is_truncated_by_char_not_bytes() {
        use super::{MAX_TAB_LABEL_CHARS, build_tab_labels};
        use crate::window::chrome::TAB_ELLIPSIS;
        use std::collections::HashMap;
        // Each emoji is 4 UTF-8 bytes but one char. A byte-slice truncator
        // would panic or produce a non-char-boundary; char-count-based
        // truncation must stay safe.
        let title = "🎉".repeat(40);
        let mut titles = HashMap::new();
        titles.insert(1usize, title);
        let labels = build_tab_labels(&[1], None, None, Some(&titles));
        let label = &labels[0];
        assert_eq!(label.chars().count(), MAX_TAB_LABEL_CHARS);
        assert!(label.starts_with("1: "));
        assert!(label.ends_with(TAB_ELLIPSIS));
    }

    #[test]
    fn build_tab_labels_exactly_at_budget_is_not_truncated() {
        use super::{MAX_TAB_LABEL_CHARS, build_tab_labels};
        use crate::window::chrome::TAB_ELLIPSIS;
        use std::collections::HashMap;
        // "1: " (3) + title → exactly MAX_TAB_LABEL_CHARS total.
        let title: String = "x".repeat(MAX_TAB_LABEL_CHARS - 3);
        let mut titles = HashMap::new();
        titles.insert(1usize, title.clone());
        let labels = build_tab_labels(&[1], None, None, Some(&titles));
        assert_eq!(labels[0], format!("1: {title}"));
        assert!(!labels[0].contains(TAB_ELLIPSIS));
        assert_eq!(labels[0].chars().count(), MAX_TAB_LABEL_CHARS);
    }

    #[test]
    fn build_tab_labels_title_takes_precedence_over_other_sources() {
        // Title beats cwd when both would be available. We can't easily
        // exercise the cwd branch without a WorkspaceManager, but we can
        // confirm that when a title exists and the workspace manager is
        // absent, the title still wins over the bare-id fallback.
        use super::build_tab_labels;
        use std::collections::HashMap;
        let mut titles = HashMap::new();
        titles.insert(3usize, "audit OSC parser".to_string());
        let labels = build_tab_labels(&[1, 3, 5], None, None, Some(&titles));
        assert_eq!(labels[0], "1");
        assert_eq!(labels[1], "3: audit OSC parser");
        assert_eq!(labels[2], "5");
    }

    #[test]
    fn build_tab_labels_rename_beats_claude_title() {
        // An in-progress inline rename must still win over a cached
        // Claude title — the user is actively editing the label.
        use super::{RenameState, build_tab_labels};
        use std::collections::HashMap;
        let state = RenameState::new(1, "manual".to_string());
        let mut titles = HashMap::new();
        titles.insert(1usize, "from claude".to_string());
        let labels = build_tab_labels(&[1], None, Some(&state), Some(&titles));
        assert_eq!(labels[0], "1: manual_");
    }
}

#[cfg(test)]
mod platform_home_tests {
    use super::platform_home_dir;

    #[test]
    fn platform_home_dir_returns_some_on_unix() {
        let home = platform_home_dir();
        assert!(
            home.is_some(),
            "platform_home_dir() must return Some on Unix"
        );
        let val = home.unwrap();
        assert!(!val.is_empty(), "home directory must not be empty");
        assert!(
            val.starts_with('/'),
            "home directory must be absolute, got: {val}"
        );
    }

    #[test]
    fn platform_home_dir_prefers_home_env() {
        if let Ok(expected) = std::env::var("HOME") {
            let got = platform_home_dir();
            assert_eq!(
                got.as_deref(),
                Some(expected.as_str()),
                "platform_home_dir should prefer $HOME"
            );
        }
    }
}
