//! Daemon lifecycle wire types and IPC protocol.
//!
//! These types are used for health checks, version handoff, session
//! management, and event subscriptions over the daemon IPC channel.
//! They are MessagePack-serialized over Unix sockets with 4-byte
//! big-endian length-prefixed framing.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{PaneId, SessionId, WorkspaceId};

/// Build hash embedded at compile time (git short hash + timestamp).
/// Informational only — not used for handoff decisions.
pub type BuildHash = String;

/// Protocol version for daemon handoff decisions.
///
/// Bump this constant when the IPC wire format or daemon behaviour changes
/// in a way that requires restarting the daemon. Normal rebuilds (UI, renderer,
/// app-side code) do **not** need a bump — the running daemon will be reused.
pub const PROTOCOL_VERSION: u32 = 5;

// ── Daemon state machine ──────────────────────────────────────────────────

/// States in the daemon lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonState {
    /// Daemon process launched, initializing subsystems.
    Starting,
    /// Binding the control socket.
    Binding,
    /// Socket bound, ready to accept connections.
    Ready,
    /// Actively serving sessions.
    Running,
    /// Graceful shutdown in progress — draining sessions.
    Draining,
    /// Daemon has stopped.
    Stopped,
}

impl std::fmt::Display for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Binding => write!(f, "binding"),
            Self::Ready => write!(f, "ready"),
            Self::Running => write!(f, "running"),
            Self::Draining => write!(f, "draining"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

// ── IPC envelope (multiplexed request/response/event) ─────────────────────

/// Maximum allowed frame payload size (1 MiB).
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// An IPC message envelope that supports request/response multiplexing
/// and server-pushed events over a single connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum IpcMessage {
    /// A request from client to daemon. The `request_id` is echoed back
    /// in the corresponding `Response` for multiplexing.
    Request {
        request_id: u64,
        payload: IpcRequest,
    },
    /// A response from daemon to client, correlated by `request_id`.
    Response {
        request_id: u64,
        payload: IpcResponse,
    },
    /// A server-pushed event to subscribed clients.
    Event { payload: DaemonEvent },
}

/// Typed IPC requests. Extends the original `DaemonRequest` (Ping/Shutdown)
/// with session management and event subscription commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum IpcRequest {
    /// Health check — daemon replies with `Pong`.
    Ping,
    /// Request graceful shutdown.
    GracefulShutdown,
    /// Subscribe to daemon events. The server will push `IpcMessage::Event`
    /// messages for the requested event kinds.
    Subscribe {
        /// Which event kinds to subscribe to. Empty = all events.
        filter: Vec<EventKind>,
    },
    /// Unsubscribe from all events on this connection.
    Unsubscribe,
    /// List active sessions.
    ListSessions,
    /// Get details about a specific session.
    GetSession { session_id: SessionId },
    /// Create a new session.
    ///
    /// `cols` and `rows` set the initial PTY dimensions. When `None`, the
    /// daemon falls back to its built-in defaults (80×24).
    CreateSession {
        name: Option<String>,
        cols: Option<u16>,
        rows: Option<u16>,
        /// Shell binary to spawn instead of the global default. When `None`,
        /// the daemon falls back to `general.shell` from config.
        shell: Option<String>,
    },
    /// Destroy a session.
    DestroySession { session_id: SessionId },
    /// Query daemon state.
    GetState,
    /// Send keys (input) to a specific pane.
    SendKeys { pane_id: PaneId, keys: Vec<u8> },
    /// Capture pane content (terminal grid snapshot).
    CapturePane { pane_id: PaneId },
    /// Split a pane (creates a new pane in the same session).
    SplitPane {
        pane_id: PaneId,
        horizontal: bool,
        /// Working directory for the new pane. Empty/None means use spawn defaults.
        cwd: Option<String>,
        /// Optional command injected into the new pane after the first shell
        /// prompt starts rendering. If the shell never emits OSC 133/633
        /// prompt marks, the daemon falls back to a short timeout.
        startup_command: Option<String>,
        /// Split ratio for the first (source) child (0.0..1.0). `None`
        /// defaults to 0.5 (equal halves). Values are clamped to
        /// `0.1..=0.9` on the daemon side to prevent degenerate layouts.
        ratio: Option<f32>,
        /// Shell binary to spawn instead of the global default. When `None`,
        /// the daemon falls back to `general.shell` from config.
        shell: Option<String>,
    },
    /// Kill (destroy) a specific pane.
    KillPane { pane_id: PaneId },
    /// Select (focus) a specific pane.
    SelectPane { pane_id: PaneId },
    /// Swap the positions of two panes within the same session's layout tree.
    ///
    /// Both panes must currently belong to the same session; the daemon
    /// rejects cross-session swaps with `IpcResponse::Error`.
    SwapPane { a: PaneId, b: PaneId },
    /// Request handoff with FD passing (Unix only).
    ///
    /// The daemon responds with `HandoffReady` containing a temporary socket
    /// path where the old daemon will send PTY FDs via SCM_RIGHTS, then
    /// initiates graceful shutdown.
    RequestHandoffFds,
    /// Set the full workspace topology for a session.
    ///
    /// The app sends this on workspace create, switch, rename, pane move,
    /// and pane close. The daemon replaces its stored workspace state for
    /// the given session with the provided list.
    SetWorkspaceState {
        session_id: SessionId,
        workspaces: Vec<WorkspaceInfo>,
        active_workspace: WorkspaceId,
    },
    /// Get the daemon's stored workspace topology for a session.
    ///
    /// Used by the app on attach to restore workspace layout, and by MCP
    /// tools to query workspace state without the app being connected.
    GetWorkspaces { session_id: SessionId },
    /// Resize a pane's PTY (rows/cols).
    ///
    /// **Stub for tn-5ps8**: the wire variant exists so the GUI's
    /// `RemotePty` backend can compile and round-trip. Server-side
    /// implementation lands in tn-5rm0 — until then the daemon answers
    /// with `IpcResponse::Error { message: "ResizePane unimplemented" }`.
    ResizePane {
        pane_id: PaneId,
        cols: u16,
        rows: u16,
    },
    /// Merge opaque key/value tags into a pane's metadata.
    ///
    /// Existing keys with the same name are overwritten; keys not present
    /// in `tags` are left untouched. Tags are opaque strings — therminal
    /// does not validate or interpret them. See tn-bbvf.
    TagPane {
        pane_id: PaneId,
        tags: HashMap<String, String>,
    },
    /// Remove tags from a pane. `keys = None` clears all tags; `Some(list)`
    /// removes only the named keys (no-op for keys that aren't set).
    UntagPane {
        pane_id: PaneId,
        keys: Option<Vec<String>>,
    },
    /// Capture a full structured snapshot of a pane's terminal state —
    /// mode flags, cursor, dimensions, and visible grid contents — so a
    /// freshly-attached GUI client can replay it onto a local `Term`
    /// before going live with `PaneOutput` events. See tn-zamd.
    CapturePaneState { pane_id: PaneId },
    /// Move a pane between workspaces inside its containing session
    /// (tn-fi1k). The pane keeps its identity and PTY — only its
    /// `WorkspaceInfo` membership changes. The daemon removes the
    /// pane from its current workspace's `pane_ids` and `LayoutSnapshot`,
    /// then appends it to the target workspace (creating the target
    /// workspace as a single-pane leaf if it doesn't exist yet).
    ///
    /// This is a metadata-only operation: the underlying PTY is not
    /// touched. Cross-session moves are not supported and return
    /// `IpcResponse::Error`.
    MovePane {
        pane_id: PaneId,
        target_workspace_id: WorkspaceId,
    },
    /// List all panes across all sessions, optionally filtered to one
    /// session. Used by the `therminal pane list` CLI subcommand
    /// (tn-k13n) so cache-sensitive callers can avoid the JSON-RPC /
    /// MCP framing overhead of `terminal.panes.list`. `session_id = None`
    /// means "all sessions".
    ListPanes { session_id: Option<SessionId> },
    /// List detected agents across all panes (tn-k13n CLI subcommand).
    ListAgents,
    /// Query recent shell commands captured by OSC 633 for a pane
    /// (tn-8ysl CLI subcommand; mirrors
    /// `terminal.semantic.query_commands`). Results are returned
    /// oldest-first within the `limit`-truncated window.
    QueryCommands {
        pane_id: PaneId,
        /// Drop blocks whose `start_line` is below this value. `0` = no
        /// filter.
        since_line: usize,
        /// Keep only the newest `limit` blocks after filtering. `0` = use
        /// the daemon default (20).
        limit: usize,
    },
    /// Switch the active workspace within a session (tn-8ysl CLI
    /// subcommand). Calls `SessionManager::set_active_workspace` on the
    /// daemon side and broadcasts `DaemonEvent::WorkspaceChanged` to
    /// subscribers.
    SwitchWorkspace {
        session_id: SessionId,
        workspace_id: WorkspaceId,
    },
    /// Create a new workspace in an existing session (tn-ceqw). The daemon
    /// appends a `WorkspaceInfo` entry with the given name (or a generated
    /// default), sets it as the active workspace, and broadcasts
    /// `WorkspaceChanged`. The new workspace is empty — callers follow up
    /// with `SplitPane` (using any existing pane as source) or `MovePane`
    /// to populate it.
    CreateWorkspace {
        session_id: SessionId,
        /// Human-readable name. `None` produces "Workspace N".
        name: Option<String>,
    },
    /// Rename an existing workspace (tn-ceqw).
    RenameWorkspace {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        name: String,
    },
    /// Query pattern engine stats (tn-86us). Returns the total number of
    /// dispatched matches plus the total loaded pattern count. Used by
    /// integration tests to assert that a pattern pack actually fired
    /// against real PTY output.
    QueryPatternStats,
}

/// Typed IPC responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "resp")]
pub enum IpcResponse {
    /// Health check response.
    Pong {
        protocol_version: u32,
        build_hash: BuildHash,
        uptime_secs: u64,
        sessions: u32,
        version: String,
    },
    /// Shutdown acknowledged.
    ShutdownAck,
    /// Subscription confirmed.
    Subscribed {
        /// The event kinds now active on this connection.
        filter: Vec<EventKind>,
    },
    /// Unsubscription confirmed.
    Unsubscribed,
    /// List of active session IDs.
    Sessions { session_ids: Vec<SessionId> },
    /// Session details.
    SessionInfo {
        session_id: SessionId,
        name: Option<String>,
        created_at_secs: u64,
    },
    /// Session created.
    SessionCreated { session_id: SessionId },
    /// Session destroyed.
    SessionDestroyed { session_id: SessionId },
    /// Current daemon state.
    State { state: DaemonState },
    /// Keys sent successfully.
    KeysSent { pane_id: PaneId },
    /// Pane content captured.
    PaneCaptured {
        pane_id: PaneId,
        lines: Vec<String>,
        cursor_col: usize,
        cursor_line: usize,
        cols: usize,
        rows: usize,
    },
    /// Pane split — new pane created.
    PaneSplit { new_pane_id: PaneId },
    /// Pane killed.
    PaneKilled { pane_id: PaneId },
    /// Pane selected (focused).
    PaneSelected { pane_id: PaneId },
    /// Two panes swapped positions in the layout tree.
    PaneSwapped { a: PaneId, b: PaneId },
    /// A pane was moved from one workspace to another (tn-fi1k).
    /// `source_workspace_id` is the workspace the pane belonged to
    /// before the move (useful for clients tracking the prior topology).
    PaneMoved {
        pane_id: PaneId,
        source_workspace_id: WorkspaceId,
        target_workspace_id: WorkspaceId,
    },
    /// Handoff ready: the old daemon has prepared FDs for transfer.
    ///
    /// The new daemon should connect to `handoff_socket` to receive the
    /// PTY master FDs and session metadata via SCM_RIGHTS.
    HandoffReady {
        /// Path to the temporary Unix socket for FD transfer.
        handoff_socket: String,
        /// Number of panes (FDs) that will be sent.
        pane_count: usize,
    },
    /// Workspace state updated.
    WorkspaceStateSet { session_id: SessionId },
    /// Workspace topology for a session.
    Workspaces {
        session_id: SessionId,
        workspaces: Vec<WorkspaceInfo>,
        active_workspace: WorkspaceId,
    },
    /// Pane resized (stub response paired with `IpcRequest::ResizePane`).
    PaneResized {
        pane_id: PaneId,
        cols: u16,
        rows: u16,
    },
    /// Structured pane state snapshot (response to `CapturePaneState`).
    PaneStateCaptured { snapshot: PaneStateSnapshot },
    /// Tags for a pane were updated (response to `TagPane` / `UntagPane`).
    /// Returns the full set of tags currently bound to the pane.
    PaneTagged {
        pane_id: PaneId,
        tags: HashMap<String, String>,
    },
    /// Pane summaries returned by `IpcRequest::ListPanes`.
    Panes { panes: Vec<PaneSummary> },
    /// Agent summaries returned by `IpcRequest::ListAgents`.
    Agents { agents: Vec<AgentSummary> },
    /// OSC 633 command history returned by `IpcRequest::QueryCommands`.
    Commands {
        pane_id: PaneId,
        commands: Vec<CommandSummary>,
    },
    /// Active workspace switched in response to `IpcRequest::SwitchWorkspace`.
    WorkspaceSwitched {
        session_id: SessionId,
        active_workspace: WorkspaceId,
    },
    /// A new workspace was created (tn-ceqw).
    WorkspaceCreated {
        session_id: SessionId,
        workspace_id: WorkspaceId,
    },
    /// A workspace was renamed (tn-ceqw).
    WorkspaceRenamed {
        session_id: SessionId,
        workspace_id: WorkspaceId,
    },
    /// Pattern engine stats snapshot (tn-86us).
    PatternStats {
        total_matches_dispatched: u64,
        total_loaded: u64,
    },
    /// Generic error response.
    Error { message: String },
}

/// Lightweight command-history entry returned by
/// `IpcRequest::QueryCommands`. Mirrors the JSON shape the
/// `terminal.semantic.query_commands` MCP tool returns so CLI and MCP
/// consumers see the same schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSummary {
    /// Command line text, if captured via an OSC 633 `E` mark.
    pub command: Option<String>,
    /// Process exit code (from the `D` mark), if finished.
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds between the `C` and `D`
    /// marks, if finished.
    pub duration_ms: Option<u64>,
    /// Grid line where the prompt started (`A` mark).
    pub start_line: usize,
    /// Grid line where execution finished (`D` mark), if any.
    pub end_line: Option<usize>,
    /// Unix epoch seconds at which the `C` (PreExec) mark was observed.
    pub started_at_secs: Option<u64>,
}

/// Lightweight pane summary returned by `IpcRequest::ListPanes`.
///
/// Mirrors the fields exposed by the `terminal.panes.list` MCP tool. Wire
/// fields are positional (rmp-serde uses array encoding for IpcMessage),
/// so we deliberately do NOT use `skip_serializing_if` here — that would
/// silently drop array slots and break round-trip decoding when other
/// optional fields follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSummary {
    pub pane_id: PaneId,
    pub session_id: SessionId,
    pub cols: u16,
    pub rows: u16,
    pub cwd: Option<String>,
    pub last_exit_code: Option<i32>,
    pub agent_name: Option<String>,
    pub tags: HashMap<String, String>,
}

/// Lightweight agent summary returned by `IpcRequest::ListAgents`.
///
/// Mirrors the fields exposed by the `terminal.agents.list` MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub pane_id: PaneId,
    pub name: String,
    pub agent_type: String,
    pub status: String,
    pub current_tool: Option<String>,
    pub detected_at: u64,
    pub pid: Option<u32>,
}

// ── PaneStateSnapshot (tn-zamd) ──────────────────────────────────────────

/// Versioned structured snapshot of a pane's terminal state.
///
/// Captured daemon-side from the alacritty `Term` and replayed GUI-side
/// onto a freshly-constructed local `Term` by synthesizing DECSET / cursor
/// position / grid paint escape sequences before live `PaneOutput`
/// forwarding begins. See tn-zamd for context.
///
/// `version` lets us add fields (e.g. colors, scrollback, keyboard
/// protocol state) without breaking wire compat with older clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneStateSnapshot {
    /// Snapshot schema version. Currently 1.
    pub version: u32,
    /// Grid columns at capture time.
    pub cols: u16,
    /// Grid rows at capture time.
    pub rows: u16,
    /// DEC private mode flags to replay as DECSET/DECRST sequences.
    pub modes: PaneModeFlags,
    /// Cursor column (0-based).
    pub cursor_col: u16,
    /// Cursor line within the visible viewport (0-based).
    pub cursor_line: u16,
    /// Visible grid contents: `rows` rows of `cols` `char`s each.
    /// Attributes are not captured in v1 — only the glyphs. Future
    /// versions may add `fg`/`bg`/`flags` per cell.
    pub grid_chars: Vec<String>,
    /// Opaque key/value tags attached to this pane (tn-bbvf).
    /// Included in the snapshot so the GUI can display tag badges in
    /// pane headers immediately on attach without a separate RPC.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,
}

/// DEC private mode flags captured from the daemon-side alacritty `Term`.
///
/// Each field mirrors a TermMode bit that affects input routing or
/// visible output (cursor visibility, mouse modes, bracketed paste, alt
/// screen, application cursor/keypad). Replayed by synthesizing the
/// matching DECSET/DECRST escape sequences on the client side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneModeFlags {
    /// DECTCEM (`?25`) — cursor visibility.
    pub show_cursor: bool,
    /// `?1` — DECCKM application cursor keys.
    pub app_cursor: bool,
    /// `?1049` — alternate screen buffer with save/restore.
    pub alt_screen: bool,
    /// `?1000` — mouse button click reporting.
    pub mouse_report_click: bool,
    /// `?1002` — mouse button + drag reporting.
    pub mouse_drag: bool,
    /// `?1003` — any mouse motion reporting.
    pub mouse_motion: bool,
    /// `?1006` — SGR mouse encoding.
    pub sgr_mouse: bool,
    /// `?2004` — bracketed paste.
    pub bracketed_paste: bool,
    /// `?1004` — focus in/out reporting.
    pub focus_in_out: bool,
    /// Numeric keypad application mode (`ESC =`).
    pub app_keypad: bool,
    /// `?7` — line wrap.
    pub line_wrap: bool,
}

/// Daemon events pushed to subscribed clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum DaemonEvent {
    /// Daemon state changed.
    StateChanged { old: DaemonState, new: DaemonState },
    /// A new session was created.
    SessionCreated { session_id: SessionId },
    /// A session was destroyed.
    SessionDestroyed { session_id: SessionId },
    /// Pane output data (for subscribed watchers).
    PaneOutput {
        session_id: SessionId,
        pane_id: PaneId,
        data: Vec<u8>,
    },
    /// Workspace topology changed for a session.
    WorkspaceChanged {
        session_id: SessionId,
        active_workspace: WorkspaceId,
    },
    /// A pane's PTY has exited (shell closed). The `RemotePty` GUI
    /// backend listens for this to tear down the local pane.
    ///
    /// **Stub for tn-5ps8**: this variant is wired through the protocol
    /// and reachable via `EventKind::PaneExited`, but the daemon does
    /// not yet broadcast it from `DaemonPtyHandler::on_eof`. That
    /// implementation lands in tn-5rm0.
    PaneExited {
        session_id: SessionId,
        pane_id: PaneId,
        exit_code: Option<i32>,
    },
    /// A pane's cell dimensions changed on the daemon side (tn-ju04).
    ///
    /// Broadcast after the daemon authoritatively resizes a pane's PTY
    /// and `Term`, including the cascading resizes triggered by
    /// `SplitPane` and `KillPane` handlers. Subscribed clients should
    /// re-read per-pane geometry (e.g. via `GetPaneGeometry` or
    /// `ListPanes`) to stay in sync. The GUI's own `ResizePane` round
    /// trips also emit this event so CLI watchers see layout changes
    /// regardless of which surface drove them.
    PaneResized {
        session_id: SessionId,
        pane_id: PaneId,
        cols: u16,
        rows: u16,
    },
}

/// Event kind discriminant for subscription filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    StateChanged,
    SessionCreated,
    SessionDestroyed,
    PaneOutput,
    WorkspaceChanged,
    PaneExited,
    /// tn-ju04: a pane's cell dimensions changed. Emitted after
    /// authoritative daemon-side resizes from `ResizePane`, `SplitPane`,
    /// and `KillPane` cascade paths.
    PaneResized,
}

impl DaemonEvent {
    /// Return the `EventKind` of this event.
    pub fn kind(&self) -> EventKind {
        match self {
            DaemonEvent::StateChanged { .. } => EventKind::StateChanged,
            DaemonEvent::SessionCreated { .. } => EventKind::SessionCreated,
            DaemonEvent::SessionDestroyed { .. } => EventKind::SessionDestroyed,
            DaemonEvent::PaneOutput { .. } => EventKind::PaneOutput,
            DaemonEvent::WorkspaceChanged { .. } => EventKind::WorkspaceChanged,
            DaemonEvent::PaneExited { .. } => EventKind::PaneExited,
            DaemonEvent::PaneResized { .. } => EventKind::PaneResized,
        }
    }
}

// ── Handoff FD-passing metadata ──────────────────────────────────────────

/// Metadata for a single pane being transferred during FD-passing handoff.
///
/// The actual PTY master file descriptor is sent out-of-band via SCM_RIGHTS;
/// this struct carries the session/pane topology and terminal dimensions so
/// the new daemon can reconstruct its `Session`/`Pane` structs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffPaneMeta {
    pub session_id: SessionId,
    pub session_name: Option<String>,
    pub pane_id: PaneId,
    pub cols: u16,
    pub rows: u16,
}

/// The full handoff payload sent from the old daemon to the new daemon.
///
/// `panes` is ordered to match the FD array sent via SCM_RIGHTS: `panes[i]`
/// corresponds to the i-th file descriptor in the ancillary data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffPayload {
    /// Ordered list of pane metadata, one per FD.
    pub panes: Vec<HandoffPaneMeta>,
}

// ── Workspace topology ──────────────────────────────────────────────────

/// Split direction for a `LayoutSnapshot::Split` node.
///
/// Mirrors `therminal-app`'s `SplitDirection` so the binary tree topology
/// can cross the IPC boundary without the app pulling protocol-internal
/// types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutSplitDirection {
    /// Children arranged side-by-side (left/right).
    Horizontal,
    /// Children stacked (top/bottom).
    Vertical,
}

/// Serializable snapshot of `therminal-app`'s `LayoutNode` binary tree.
///
/// The app keeps the live tree (with `PaneState` payloads, viewport rects,
/// etc.) private; only this lightweight projection — direction, ratio, and
/// leaf pane IDs — crosses crate boundaries so the daemon can answer
/// `terminal.workspaces.get_layout` without a degraded shim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutSnapshot {
    /// A terminal pane leaf.
    Leaf {
        /// The pane ID for this leaf.
        pane_id: PaneId,
    },
    /// A split node containing two children.
    Split {
        /// Direction of the split.
        direction: LayoutSplitDirection,
        /// First child's share of the parent extent (0.0..1.0).
        ratio: f32,
        /// First child (left or top).
        first: Box<LayoutSnapshot>,
        /// Second child (right or bottom).
        second: Box<LayoutSnapshot>,
    },
}

/// Metadata for a single workspace (tab) within a session.
///
/// The daemon stores this so MCP tools can query workspace topology
/// without the app being connected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// Workspace slot number (1-9).
    pub id: WorkspaceId,
    /// Human-readable workspace name (e.g. "build", "logs").
    pub name: String,
    /// Display order (lower = leftmost tab). Usually matches `id`.
    pub order: u32,
    /// Pane IDs currently assigned to this workspace.
    pub pane_ids: Vec<PaneId>,
    /// The focused pane within this workspace, if any.
    pub focused_pane: Option<PaneId>,
    /// Real binary layout tree (directions + ratios). `None` for legacy
    /// callers; consumers fall back to a degraded cascade in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutSnapshot>,
}

// ── Persisted state (session/layout persistence across daemon restarts) ──

/// Persisted metadata for a single pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPane {
    /// Last known working directory (empty if unknown).
    pub cwd: String,
    /// Shell command that was used (empty = default shell).
    pub shell: String,
    /// Terminal columns at time of save.
    pub cols: u16,
    /// Terminal rows at time of save.
    pub rows: u16,
    /// Opaque key/value tags attached to this pane (tn-bbvf). Survives
    /// daemon restarts via session-state persistence.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,
}

/// Persisted metadata for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Human-readable session name, if any.
    pub name: Option<String>,
    /// Panes in this session (flat list; layout topology preserved by order).
    pub panes: Vec<PersistedPane>,
    /// Workspace topology at time of save. Empty for legacy data.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
    /// Which workspace was active at time of save (0 = unknown/legacy).
    #[serde(default)]
    pub active_workspace: WorkspaceId,
}

/// Top-level persisted daemon state, serialised to `sessions.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    /// All sessions that were active when state was last saved.
    pub sessions: Vec<PersistedSession>,
}

impl PaneStateSnapshot {
    /// Current snapshot schema version.
    pub const CURRENT_VERSION: u32 = 1;

    /// Synthesize a stream of VT escape sequences that, when fed through
    /// an alacritty `Term`, recreates the captured state: mode flags,
    /// cleared screen, grid contents, and cursor position.
    ///
    /// This is the tn-zamd replay path. Emitting escape sequences rather
    /// than poking at `TermMode` bits directly lets the local `Term`'s
    /// existing DECSET/DECRST handling apply the state, which means any
    /// new flag added to [`PaneModeFlags`] Just Works as long as the
    /// caller updates this method.
    pub fn to_replay_bytes(&self) -> Vec<u8> {
        let mut out: Vec<u8> =
            Vec::with_capacity(64 + self.grid_chars.iter().map(|r| r.len() + 8).sum::<usize>());

        // Helper: emit DECSET (h) or DECRST (l) for a numeric param.
        let emit_mode = |buf: &mut Vec<u8>, param: &str, on: bool| {
            buf.extend_from_slice(b"\x1b[?");
            buf.extend_from_slice(param.as_bytes());
            buf.push(if on { b'h' } else { b'l' });
        };

        let m = &self.modes;
        // Cursor visibility.
        emit_mode(&mut out, "25", m.show_cursor);
        // App cursor keys.
        emit_mode(&mut out, "1", m.app_cursor);
        // Line wrap.
        emit_mode(&mut out, "7", m.line_wrap);
        // Focus in/out.
        emit_mode(&mut out, "1004", m.focus_in_out);
        // Bracketed paste.
        emit_mode(&mut out, "2004", m.bracketed_paste);
        // Mouse modes ?1000/?1002/?1003 share a single mutually-exclusive
        // bitmask in alacritty, so emit only the highest active mode. A
        // fresh local Term defaults to all-off, so no DECRSTs are needed.
        if m.mouse_motion {
            emit_mode(&mut out, "1003", true);
        } else if m.mouse_drag {
            emit_mode(&mut out, "1002", true);
        } else if m.mouse_report_click {
            emit_mode(&mut out, "1000", true);
        }
        emit_mode(&mut out, "1006", m.sgr_mouse);
        // Alt screen (after mouse so the screen we paint into is correct).
        emit_mode(&mut out, "1049", m.alt_screen);
        // Application keypad (ESC = / ESC >).
        if m.app_keypad {
            out.extend_from_slice(b"\x1b=");
        } else {
            out.extend_from_slice(b"\x1b>");
        }

        // Home cursor. We intentionally skip ESC[2J (clear screen)
        // because it pushes the blank viewport into scrollback history
        // on a freshly-created local Term, creating spurious scrollback
        // rows. The row-by-row CUP painting below overwrites every cell
        // anyway, so clearing first is unnecessary.
        out.extend_from_slice(b"\x1b[H");

        // Paint grid rows. Use CUP (ESC[<row>;<col>H) per row, 1-based.
        for (i, row) in self.grid_chars.iter().enumerate() {
            let line_no = i + 1;
            out.extend_from_slice(format!("\x1b[{line_no};1H").as_bytes());
            // Sanitize: replace any C0/DEL/C1 control chars with space so a
            // stray ESC (or other control) in the captured grid can't be
            // re-interpreted by the local Term's VTE parser as the start of
            // an escape sequence on replay.
            for c in row.chars() {
                let cp = c as u32;
                let safe = if cp < 0x20 || cp == 0x7F || (0x80..=0x9F).contains(&cp) {
                    ' '
                } else {
                    c
                };
                let mut buf = [0u8; 4];
                out.extend_from_slice(safe.encode_utf8(&mut buf).as_bytes());
            }
        }

        // Final cursor position (1-based).
        let cl = self.cursor_line as usize + 1;
        let cc = self.cursor_col as usize + 1;
        out.extend_from_slice(format!("\x1b[{cl};{cc}H").as_bytes());

        out
    }
}

// ── Framing helpers ───────────────────────────────────────────────────────

/// Error type for frame encoding operations.
#[derive(Debug)]
pub enum EncodeFrameError {
    /// MessagePack serialization failed.
    Serialize(rmp_serde::encode::Error),
    /// Payload exceeds `MAX_FRAME_SIZE`.
    FrameTooLarge(usize),
}

impl std::fmt::Display for EncodeFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(e) => write!(f, "serialization error: {e}"),
            Self::FrameTooLarge(size) => {
                write!(
                    f,
                    "frame payload too large: {size} bytes (max {MAX_FRAME_SIZE})"
                )
            }
        }
    }
}

impl std::error::Error for EncodeFrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(e) => Some(e),
            Self::FrameTooLarge(_) => None,
        }
    }
}

impl From<rmp_serde::encode::Error> for EncodeFrameError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        Self::Serialize(e)
    }
}

// ── IPC frame helpers ─────────────────────────────────────────────────────

/// Encode an `IpcMessage` into a length-prefixed frame (4-byte BE length + MessagePack payload).
pub fn encode_ipc(msg: &IpcMessage) -> Result<Vec<u8>, EncodeFrameError> {
    let payload = rmp_serde::to_vec(msg)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(EncodeFrameError::FrameTooLarge(payload.len()));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode an `IpcMessage` from a MessagePack payload (without length prefix).
pub fn decode_ipc(data: &[u8]) -> Result<IpcMessage, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_state_display() {
        assert_eq!(DaemonState::Starting.to_string(), "starting");
        assert_eq!(DaemonState::Running.to_string(), "running");
        assert_eq!(DaemonState::Draining.to_string(), "draining");
        assert_eq!(DaemonState::Stopped.to_string(), "stopped");
    }

    #[test]
    fn ipc_request_ping_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 42,
            payload: IpcRequest::Ping,
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_response_pong_round_trip() {
        let msg = IpcMessage::Response {
            request_id: 42,
            payload: IpcResponse::Pong {
                protocol_version: 1,
                build_hash: "abc123".into(),
                uptime_secs: 100,
                sessions: 2,
                version: "0.1.0".into(),
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_event_round_trip() {
        let msg = IpcMessage::Event {
            payload: DaemonEvent::StateChanged {
                old: DaemonState::Ready,
                new: DaemonState::Running,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_subscribe_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 1,
            payload: IpcRequest::Subscribe {
                filter: vec![EventKind::StateChanged, EventKind::SessionCreated],
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_frame_length_prefix() {
        let msg = IpcMessage::Request {
            request_id: 0,
            payload: IpcRequest::GetState,
        };
        let encoded = encode_ipc(&msg).unwrap();
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(len, encoded.len() - 4);
    }

    #[test]
    fn daemon_event_kind() {
        let e = DaemonEvent::SessionCreated { session_id: 1 };
        assert_eq!(e.kind(), EventKind::SessionCreated);
    }

    #[test]
    fn move_pane_request_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 99,
            payload: IpcRequest::MovePane {
                pane_id: 17,
                target_workspace_id: 3,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn pane_moved_response_round_trip() {
        let msg = IpcMessage::Response {
            request_id: 99,
            payload: IpcResponse::PaneMoved {
                pane_id: 17,
                source_workspace_id: 1,
                target_workspace_id: 3,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_pane_output_round_trip() {
        let msg = IpcMessage::Event {
            payload: DaemonEvent::PaneOutput {
                session_id: 1,
                pane_id: 1,
                data: vec![0x1b, b'[', b'H'],
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn set_workspace_state_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 7,
            payload: IpcRequest::SetWorkspaceState {
                session_id: 1,
                workspaces: vec![
                    WorkspaceInfo {
                        id: 1,
                        name: "main".into(),
                        order: 0,
                        pane_ids: vec![10, 11],
                        focused_pane: Some(10),
                        layout: None,
                    },
                    WorkspaceInfo {
                        id: 3,
                        name: "logs".into(),
                        order: 1,
                        pane_ids: vec![20],
                        focused_pane: Some(20),
                        layout: None,
                    },
                ],
                active_workspace: 1,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_workspaces_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 8,
            payload: IpcRequest::GetWorkspaces { session_id: 1 },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn workspaces_response_round_trip() {
        let msg = IpcMessage::Response {
            request_id: 8,
            payload: IpcResponse::Workspaces {
                session_id: 1,
                workspaces: vec![WorkspaceInfo {
                    id: 1,
                    name: "default".into(),
                    order: 0,
                    pane_ids: vec![5],
                    focused_pane: Some(5),
                    layout: None,
                }],
                active_workspace: 1,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn workspace_changed_event_round_trip() {
        let msg = IpcMessage::Event {
            payload: DaemonEvent::WorkspaceChanged {
                session_id: 1,
                active_workspace: 3,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn workspace_changed_event_kind() {
        let e = DaemonEvent::WorkspaceChanged {
            session_id: 1,
            active_workspace: 2,
        };
        assert_eq!(e.kind(), EventKind::WorkspaceChanged);
    }

    #[test]
    fn workspace_info_serde_json_round_trip() {
        let info = WorkspaceInfo {
            id: 2,
            name: "build".into(),
            order: 1,
            pane_ids: vec![100, 200],
            focused_pane: Some(100),
            layout: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: WorkspaceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, parsed);
    }

    #[test]
    fn persisted_session_workspace_defaults() {
        // Legacy JSON without workspace fields should deserialize with defaults.
        let json = r#"{"name":"old","panes":[]}"#;
        let session: PersistedSession = serde_json::from_str(json).unwrap();
        assert!(session.workspaces.is_empty());
        assert_eq!(session.active_workspace, 0);
    }

    #[test]
    fn persisted_session_with_workspaces_round_trip() {
        let session = PersistedSession {
            name: Some("test".into()),
            panes: vec![],
            workspaces: vec![WorkspaceInfo {
                id: 1,
                name: "main".into(),
                order: 0,
                pane_ids: vec![1],
                focused_pane: Some(1),
                layout: None,
            }],
            active_workspace: 1,
        };
        let json = serde_json::to_string(&session).unwrap();
        let parsed: PersistedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.workspaces.len(), 1);
        assert_eq!(parsed.active_workspace, 1);
        assert_eq!(parsed.workspaces[0].name, "main");
    }

    // ── LayoutSnapshot ──────────────────────────────────────────────────

    fn fixture_snapshot() -> LayoutSnapshot {
        LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.6,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.25,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
            }),
        }
    }

    #[test]
    fn layout_snapshot_json_round_trip() {
        let snap = fixture_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: LayoutSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, parsed);
    }

    #[test]
    fn layout_snapshot_msgpack_round_trip() {
        let snap = fixture_snapshot();
        let bytes = rmp_serde::to_vec(&snap).unwrap();
        let parsed: LayoutSnapshot = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(snap, parsed);
    }

    #[test]
    fn layout_snapshot_ratios_within_bounds() {
        fn walk(node: &LayoutSnapshot) {
            if let LayoutSnapshot::Split {
                ratio,
                first,
                second,
                ..
            } = node
            {
                assert!((0.0..=1.0).contains(ratio), "ratio out of bounds: {ratio}");
                walk(first);
                walk(second);
            }
        }
        walk(&fixture_snapshot());
    }

    #[test]
    fn pane_state_snapshot_msgpack_round_trip() {
        let snap = PaneStateSnapshot {
            version: PaneStateSnapshot::CURRENT_VERSION,
            cols: 80,
            rows: 24,
            modes: PaneModeFlags {
                show_cursor: false,
                mouse_report_click: true,
                mouse_drag: true,
                sgr_mouse: true,
                bracketed_paste: true,
                line_wrap: true,
                ..Default::default()
            },
            cursor_col: 10,
            cursor_line: 5,
            grid_chars: vec!["hello".into(); 24],
            tags: HashMap::new(),
        };
        let msg = IpcMessage::Response {
            request_id: 42,
            payload: IpcResponse::PaneStateCaptured {
                snapshot: snap.clone(),
            },
        };
        let bytes = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&bytes[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn pane_state_snapshot_replay_bytes_contain_decset() {
        let snap = PaneStateSnapshot {
            version: 1,
            cols: 5,
            rows: 2,
            modes: PaneModeFlags {
                show_cursor: false,
                mouse_report_click: true,
                sgr_mouse: true,
                ..Default::default()
            },
            cursor_col: 0,
            cursor_line: 0,
            grid_chars: vec!["hello".into(), "world".into()],
            tags: HashMap::new(),
        };
        let bytes = snap.to_replay_bytes();
        // Hide cursor.
        assert!(bytes.windows(6).any(|w| w == b"\x1b[?25l"));
        // Mouse click on.
        assert!(bytes.windows(8).any(|w| w == b"\x1b[?1000h"));
        // SGR mouse on.
        assert!(bytes.windows(8).any(|w| w == b"\x1b[?1006h"));
        // Home cursor (ESC[2J intentionally removed to avoid scrollback pollution).
        assert!(bytes.windows(3).any(|w| w == b"\x1b[H"));
    }

    #[test]
    fn tag_pane_request_round_trip() {
        let mut tags = HashMap::new();
        tags.insert("issue_id".to_string(), "tn-bbvf".to_string());
        tags.insert("branch".to_string(), "feat/tags".to_string());
        let msg = IpcMessage::Request {
            request_id: 11,
            payload: IpcRequest::TagPane {
                pane_id: 7,
                tags: tags.clone(),
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn untag_pane_request_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 12,
            payload: IpcRequest::UntagPane {
                pane_id: 7,
                keys: Some(vec!["issue_id".into()]),
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);

        let msg_clear = IpcMessage::Request {
            request_id: 13,
            payload: IpcRequest::UntagPane {
                pane_id: 7,
                keys: None,
            },
        };
        let encoded = encode_ipc(&msg_clear).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg_clear, decoded);
    }

    #[test]
    fn pane_tagged_response_round_trip() {
        let mut tags = HashMap::new();
        tags.insert("k".to_string(), "v".to_string());
        let msg = IpcMessage::Response {
            request_id: 14,
            payload: IpcResponse::PaneTagged { pane_id: 7, tags },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn persisted_pane_tags_round_trip() {
        let mut tags = HashMap::new();
        tags.insert("issue_id".to_string(), "tn-bbvf".to_string());
        let pane = PersistedPane {
            cwd: "/tmp".into(),
            shell: String::new(),
            cols: 80,
            rows: 24,
            tags,
        };
        let json = serde_json::to_string(&pane).unwrap();
        let parsed: PersistedPane = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.tags.get("issue_id").map(String::as_str),
            Some("tn-bbvf")
        );

        // Legacy data without `tags` should still parse.
        let legacy = r#"{"cwd":"/x","shell":"","cols":80,"rows":24}"#;
        let parsed: PersistedPane = serde_json::from_str(legacy).unwrap();
        assert!(parsed.tags.is_empty());
    }

    #[test]
    fn workspace_info_with_layout_round_trip() {
        let info = WorkspaceInfo {
            id: 1,
            name: "main".into(),
            order: 0,
            pane_ids: vec![1, 2, 3],
            focused_pane: Some(2),
            layout: Some(fixture_snapshot()),
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: WorkspaceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, parsed);
        assert!(parsed.layout.is_some());
    }

    #[test]
    fn ipc_swap_pane_request_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 99,
            payload: IpcRequest::SwapPane { a: 10, b: 20 },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_pane_swapped_response_round_trip() {
        let msg = IpcMessage::Response {
            request_id: 99,
            payload: IpcResponse::PaneSwapped { a: 10, b: 20 },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_list_panes_request_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 100,
            payload: IpcRequest::ListPanes {
                session_id: Some(1),
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);

        let msg_all = IpcMessage::Request {
            request_id: 101,
            payload: IpcRequest::ListPanes { session_id: None },
        };
        let encoded = encode_ipc(&msg_all).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg_all, decoded);
    }

    #[test]
    fn ipc_panes_response_round_trip() {
        let mut tags = HashMap::new();
        tags.insert("worker".to_string(), "alice".to_string());
        let panes = vec![
            PaneSummary {
                pane_id: 1,
                session_id: 1,
                cols: 80,
                rows: 24,
                cwd: Some("/tmp".to_string()),
                last_exit_code: Some(0),
                agent_name: Some("claude".to_string()),
                tags: tags.clone(),
            },
            PaneSummary {
                pane_id: 2,
                session_id: 1,
                cols: 100,
                rows: 30,
                cwd: None,
                last_exit_code: None,
                agent_name: None,
                tags: HashMap::new(),
            },
        ];
        let msg = IpcMessage::Response {
            request_id: 100,
            payload: IpcResponse::Panes { panes },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_list_agents_request_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 200,
            payload: IpcRequest::ListAgents,
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_agents_response_round_trip() {
        let agents = vec![AgentSummary {
            pane_id: 1,
            name: "claude".to_string(),
            agent_type: "claude".to_string(),
            status: "active".to_string(),
            current_tool: None,
            detected_at: 1234567890,
            pid: Some(99),
        }];
        let msg = IpcMessage::Response {
            request_id: 200,
            payload: IpcResponse::Agents { agents },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }
}
