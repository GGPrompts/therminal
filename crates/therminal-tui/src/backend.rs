//! Daemon IPC backend — async adapter wrapping `DaemonClient`.
//!
//! The TUI event loop is synchronous (crossterm polling). This module
//! provides a thin synchronous facade over the async daemon client by
//! running IPC calls on a dedicated tokio runtime in a background thread
//! and exchanging results via channels.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use therminal_daemon_client::DaemonClient;
use therminal_protocol::daemon::{
    AgentSummary, DaemonEvent, EventKind, IpcRequest, IpcResponse, PaneSummary,
};
use therminal_protocol::{PaneId, SessionId};

/// Request types the TUI sends to the background worker.
#[allow(dead_code)]
enum BackendRequest {
    Ping,
    ListSessions,
    ListPanes { session_id: Option<SessionId> },
    ListAgents,
    CapturePane { pane_id: PaneId },
    GetState,
    Subscribe { filter: Vec<EventKind> },
    PollEvent,
    Shutdown,
}

/// Response types the background worker sends back.
#[derive(Debug)]
#[allow(dead_code)]
pub enum BackendResponse {
    Pong {
        version: String,
        uptime_secs: u64,
        sessions: u32,
    },
    Sessions {
        session_ids: Vec<SessionId>,
    },
    SessionInfo {
        session_id: SessionId,
        name: Option<String>,
    },
    Panes {
        panes: Vec<PaneSummary>,
    },
    Agents {
        agents: Vec<AgentSummary>,
    },
    PaneCaptured {
        pane_id: PaneId,
        lines: Vec<String>,
        cols: usize,
        rows: usize,
    },
    State {
        state: String,
    },
    Subscribed,
    Event(DaemonEvent),
    NoEvent,
    Error(String),
}

/// Synchronous handle to the daemon backend.
///
/// The TUI holds this and calls methods that block until the IPC
/// round-trip completes (with a timeout).
pub struct DaemonBackend {
    req_tx: mpsc::Sender<(BackendRequest, mpsc::Sender<BackendResponse>)>,
}

impl DaemonBackend {
    /// Connect to the daemon at `socket_path`. Spawns a background thread
    /// with a tokio runtime for async IPC.
    pub fn connect(socket_path: &Path) -> Result<Self> {
        let socket = socket_path.to_path_buf();
        let (req_tx, req_rx) = mpsc::channel::<(BackendRequest, mpsc::Sender<BackendResponse>)>();

        std::thread::Builder::new()
            .name("tui-ipc".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime for TUI IPC");
                rt.block_on(ipc_worker(socket, req_rx));
            })
            .context("failed to spawn IPC worker thread")?;

        Ok(Self { req_tx })
    }

    /// Send a request and wait for the response (up to 5s).
    fn request(&self, req: BackendRequest) -> BackendResponse {
        let (resp_tx, resp_rx) = mpsc::channel();
        if self.req_tx.send((req, resp_tx)).is_err() {
            return BackendResponse::Error("IPC worker shut down".into());
        }
        resp_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or(BackendResponse::Error("IPC timeout".into()))
    }

    pub fn ping(&self) -> BackendResponse {
        self.request(BackendRequest::Ping)
    }

    pub fn list_sessions(&self) -> BackendResponse {
        self.request(BackendRequest::ListSessions)
    }

    pub fn list_panes(&self, session_id: Option<SessionId>) -> BackendResponse {
        self.request(BackendRequest::ListPanes { session_id })
    }

    pub fn list_agents(&self) -> BackendResponse {
        self.request(BackendRequest::ListAgents)
    }

    pub fn capture_pane(&self, pane_id: PaneId) -> BackendResponse {
        self.request(BackendRequest::CapturePane { pane_id })
    }

    #[allow(dead_code)]
    pub fn get_state(&self) -> BackendResponse {
        self.request(BackendRequest::GetState)
    }

    #[allow(dead_code)]
    pub fn subscribe(&self, filter: Vec<EventKind>) -> BackendResponse {
        self.request(BackendRequest::Subscribe { filter })
    }

    #[allow(dead_code)]
    pub fn poll_event(&self) -> BackendResponse {
        self.request(BackendRequest::PollEvent)
    }
}

impl Drop for DaemonBackend {
    fn drop(&mut self) {
        let (resp_tx, _) = mpsc::channel();
        let _ = self.req_tx.send((BackendRequest::Shutdown, resp_tx));
    }
}

/// Async worker that owns the `DaemonClient` connection.
async fn ipc_worker(
    socket: PathBuf,
    req_rx: mpsc::Receiver<(BackendRequest, mpsc::Sender<BackendResponse>)>,
) {
    let client = match DaemonClient::connect(&socket).await {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to connect to daemon: {e}");
            // Drain requests with error responses until the TUI gives up.
            while let Ok((_, resp_tx)) = req_rx.recv() {
                let _ = resp_tx.send(BackendResponse::Error(format!("daemon not available: {e}")));
            }
            return;
        }
    };

    debug!("connected to daemon at {}", socket.display());

    while let Ok((req, resp_tx)) = req_rx.recv() {
        let response = match req {
            BackendRequest::Shutdown => break,
            BackendRequest::Ping => match client.ping().await {
                Ok(IpcResponse::Pong {
                    version,
                    uptime_secs,
                    sessions,
                    ..
                }) => BackendResponse::Pong {
                    version,
                    uptime_secs,
                    sessions,
                },
                Ok(other) => BackendResponse::Error(format!("unexpected: {other:?}")),
                Err(e) => BackendResponse::Error(format!("{e}")),
            },
            BackendRequest::ListSessions => {
                match client.send_request(IpcRequest::ListSessions).await {
                    Ok(IpcResponse::Sessions { session_ids }) => {
                        BackendResponse::Sessions { session_ids }
                    }
                    Ok(other) => BackendResponse::Error(format!("unexpected: {other:?}")),
                    Err(e) => BackendResponse::Error(format!("{e}")),
                }
            }
            BackendRequest::ListPanes { session_id } => {
                match client
                    .send_request(IpcRequest::ListPanes { session_id })
                    .await
                {
                    Ok(IpcResponse::Panes { panes }) => BackendResponse::Panes { panes },
                    Ok(other) => BackendResponse::Error(format!("unexpected: {other:?}")),
                    Err(e) => BackendResponse::Error(format!("{e}")),
                }
            }
            BackendRequest::ListAgents => match client.send_request(IpcRequest::ListAgents).await {
                Ok(IpcResponse::Agents { agents }) => BackendResponse::Agents { agents },
                Ok(other) => BackendResponse::Error(format!("unexpected: {other:?}")),
                Err(e) => BackendResponse::Error(format!("{e}")),
            },
            BackendRequest::CapturePane { pane_id } => {
                match client
                    .send_request(IpcRequest::CapturePane { pane_id })
                    .await
                {
                    Ok(IpcResponse::PaneCaptured {
                        pane_id,
                        lines,
                        cols,
                        rows,
                        ..
                    }) => BackendResponse::PaneCaptured {
                        pane_id,
                        lines,
                        cols,
                        rows,
                    },
                    Ok(other) => BackendResponse::Error(format!("unexpected: {other:?}")),
                    Err(e) => BackendResponse::Error(format!("{e}")),
                }
            }
            BackendRequest::GetState => match client.get_state().await {
                Ok(IpcResponse::State { state }) => BackendResponse::State {
                    state: state.to_string(),
                },
                Ok(other) => BackendResponse::Error(format!("unexpected: {other:?}")),
                Err(e) => BackendResponse::Error(format!("{e}")),
            },
            BackendRequest::Subscribe { filter } => match client.subscribe_events(filter).await {
                Ok(IpcResponse::Subscribed { .. }) => BackendResponse::Subscribed,
                Ok(other) => BackendResponse::Error(format!("unexpected: {other:?}")),
                Err(e) => BackendResponse::Error(format!("{e}")),
            },
            BackendRequest::PollEvent => match client.try_recv_event() {
                Some(event) => BackendResponse::Event(event),
                None => BackendResponse::NoEvent,
            },
        };
        let _ = resp_tx.send(response);
    }

    client.close().await;
    debug!("IPC worker shut down");
}
