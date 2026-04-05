//! Daemon lifecycle state machine.
//!
//! Manages transitions: Starting -> Binding -> Ready -> Running -> Draining -> Stopped.
//! The state machine is driven by events from the control socket and internal timers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{watch, Notify};
use tracing::{info, warn};

use therminal_protocol::DaemonState;

/// Configuration for daemon lifecycle behavior.
#[derive(Debug, Clone)]
pub struct LifecycleConfig {
    /// How long to keep the daemon alive after the last session closes.
    /// Set to `None` to disable idle exit (daemon runs until explicitly stopped).
    pub keep_alive: Option<Duration>,
    /// Timeout for draining sessions during graceful shutdown.
    pub drain_timeout: Duration,
    /// Timeout for handoff to a new daemon.
    pub handoff_timeout: Duration,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            keep_alive: Some(Duration::from_secs(300)), // 5 minutes
            drain_timeout: Duration::from_secs(30),
            handoff_timeout: Duration::from_secs(5),
        }
    }
}

/// The daemon lifecycle state machine.
///
/// Thread-safe: state changes are broadcast via a `watch` channel so any
/// number of tasks can observe transitions.
pub struct Lifecycle {
    state_tx: watch::Sender<DaemonState>,
    state_rx: watch::Receiver<DaemonState>,
    config: LifecycleConfig,
    started_at: Instant,
    shutdown_notify: Arc<Notify>,
    session_count: tokio::sync::watch::Sender<u32>,
    session_count_rx: tokio::sync::watch::Receiver<u32>,
}

impl Lifecycle {
    /// Create a new lifecycle in the `Starting` state.
    pub fn new(config: LifecycleConfig) -> Self {
        let (state_tx, state_rx) = watch::channel(DaemonState::Starting);
        let (session_count, session_count_rx) = watch::channel(0u32);
        Self {
            state_tx,
            state_rx,
            config,
            started_at: Instant::now(),
            shutdown_notify: Arc::new(Notify::new()),
            session_count,
            session_count_rx,
        }
    }

    /// Get the current state.
    pub fn state(&self) -> DaemonState {
        *self.state_rx.borrow()
    }

    /// Subscribe to state changes.
    pub fn watch_state(&self) -> watch::Receiver<DaemonState> {
        self.state_rx.clone()
    }

    /// Get a handle to the shutdown notifier.
    pub fn shutdown_notify(&self) -> Arc<Notify> {
        self.shutdown_notify.clone()
    }

    /// Get uptime in seconds.
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Get current session count.
    pub fn session_count(&self) -> u32 {
        *self.session_count_rx.borrow()
    }

    /// Update session count. Used by the session manager.
    pub fn set_session_count(&self, count: u32) {
        let _ = self.session_count.send(count);
    }

    /// Transition to a new state. Returns `Err` if the transition is invalid.
    pub fn transition(&self, new_state: DaemonState) -> anyhow::Result<()> {
        let current = self.state();
        if !is_valid_transition(current, new_state) {
            anyhow::bail!("invalid state transition: {} -> {}", current, new_state);
        }
        info!(from = %current, to = %new_state, "daemon state transition");
        let _ = self.state_tx.send(new_state);

        if new_state == DaemonState::Stopped {
            self.shutdown_notify.notify_waiters();
        }

        Ok(())
    }

    /// Initiate graceful shutdown. Transitions to Draining, then Stopped
    /// after sessions drain or timeout.
    pub async fn initiate_shutdown(&self) -> anyhow::Result<()> {
        let current = self.state();
        if current == DaemonState::Draining || current == DaemonState::Stopped {
            return Ok(());
        }

        self.transition(DaemonState::Draining)?;

        // Wait for sessions to drain or timeout
        let drain_timeout = self.config.drain_timeout;
        let mut session_rx = self.session_count_rx.clone();

        let drained = tokio::time::timeout(drain_timeout, async {
            while *session_rx.borrow_and_update() > 0 {
                if session_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;

        if drained.is_err() {
            warn!(
                sessions = self.session_count(),
                "drain timeout reached, forcing shutdown"
            );
        }

        self.transition(DaemonState::Stopped)?;
        Ok(())
    }

    /// Spawn an idle-exit watcher task. When sessions drop to zero and stay
    /// there for `keep_alive` duration, initiates shutdown.
    pub fn spawn_idle_watcher(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let keep_alive = self.config.keep_alive?;
        let lifecycle = Arc::clone(self);

        Some(tokio::spawn(async move {
            let mut session_rx = lifecycle.session_count_rx.clone();
            loop {
                // Wait until sessions drop to zero
                while *session_rx.borrow_and_update() > 0 {
                    if session_rx.changed().await.is_err() {
                        return;
                    }
                }

                // Sessions are zero — start the keep-alive timer
                info!(
                    keep_alive_secs = keep_alive.as_secs(),
                    "no active sessions, starting idle timer"
                );
                let timeout = tokio::time::timeout(keep_alive, async {
                    // Wait for session count to change (i.e., new session arrives)
                    let _ = session_rx.changed().await;
                })
                .await;

                if timeout.is_err() {
                    // Timer expired without new sessions
                    if *session_rx.borrow() == 0 {
                        info!("idle timeout reached, shutting down");
                        let _ = lifecycle.initiate_shutdown().await;
                        return;
                    }
                }
                // Otherwise, a session appeared — loop back
            }
        }))
    }

    /// Get the lifecycle config.
    pub fn config(&self) -> &LifecycleConfig {
        &self.config
    }
}

/// Check if a state transition is valid.
fn is_valid_transition(from: DaemonState, to: DaemonState) -> bool {
    use DaemonState::*;
    matches!(
        (from, to),
        (Starting, Binding)
            | (Binding, Ready)
            | (Ready, Running)
            | (Running, Draining)
            | (Ready, Draining) // Shutdown before any sessions
            | (Draining, Stopped)
            // Allow direct jumps for error paths
            | (Starting, Stopped)
            | (Binding, Stopped)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        use DaemonState::*;
        assert!(is_valid_transition(Starting, Binding));
        assert!(is_valid_transition(Binding, Ready));
        assert!(is_valid_transition(Ready, Running));
        assert!(is_valid_transition(Running, Draining));
        assert!(is_valid_transition(Draining, Stopped));
        assert!(is_valid_transition(Starting, Stopped));
    }

    #[test]
    fn invalid_transitions() {
        use DaemonState::*;
        assert!(!is_valid_transition(Starting, Running));
        assert!(!is_valid_transition(Stopped, Starting));
        assert!(!is_valid_transition(Running, Ready));
        assert!(!is_valid_transition(Draining, Running));
    }

    #[tokio::test]
    async fn lifecycle_basic_transitions() {
        let lc = Lifecycle::new(LifecycleConfig::default());
        assert_eq!(lc.state(), DaemonState::Starting);
        lc.transition(DaemonState::Binding).unwrap();
        assert_eq!(lc.state(), DaemonState::Binding);
        lc.transition(DaemonState::Ready).unwrap();
        lc.transition(DaemonState::Running).unwrap();
        lc.transition(DaemonState::Draining).unwrap();
        lc.transition(DaemonState::Stopped).unwrap();
        assert_eq!(lc.state(), DaemonState::Stopped);
    }

    #[tokio::test]
    async fn lifecycle_invalid_transition_errors() {
        let lc = Lifecycle::new(LifecycleConfig::default());
        assert!(lc.transition(DaemonState::Running).is_err());
    }

    #[test]
    fn uptime_increases() {
        let lc = Lifecycle::new(LifecycleConfig::default());
        // Just verify it doesn't panic; uptime may be 0
        let _ = lc.uptime_secs();
    }

    #[test]
    fn session_count_updates() {
        let lc = Lifecycle::new(LifecycleConfig::default());
        assert_eq!(lc.session_count(), 0);
        lc.set_session_count(3);
        assert_eq!(lc.session_count(), 3);
    }
}
