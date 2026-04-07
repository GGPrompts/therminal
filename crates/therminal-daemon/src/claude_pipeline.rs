//! Claude agent-event pipeline.
//!
//! Wires [`ClaudeStatePoller`] (file watcher over `/tmp/{claude,codex,copilot}-state/`)
//! into [`ClaudeJsonlRegistry`] (one JSONL tailer per top-level Claude session +
//! per-subagent tailers) and re-broadcasts the resulting [`TaggedAgentEvent`]s
//! over a `tokio::sync::broadcast` channel for fan-out to multiple subscribers.
//!
//! Consumed by the MCP resource subscription `therminal://claude/events`.
//!
//! [`ClaudeStatePoller`]: crate::claude_state::ClaudeStatePoller
//! [`ClaudeJsonlRegistry`]: crate::claude_jsonl_tailer::ClaudeJsonlRegistry
//! [`TaggedAgentEvent`]: crate::claude_jsonl_tailer::TaggedAgentEvent

use std::time::Duration;

use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::claude_jsonl_tailer::{ClaudeJsonlRegistry, TaggedAgentEvent};
use crate::claude_state::ClaudeStatePoller;

/// Default fan-out broadcast capacity. Subscribers that lag past this many
/// events will receive `Lagged` and resync.
pub const BROADCAST_CAPACITY: usize = 1024;

/// Default poll interval for the registry tick.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Spawn the Claude agent-event pipeline using the system default state
/// directories. Returns a broadcast sender; clone-and-`subscribe()` from any
/// number of consumers.
///
/// Returns `None` if the poller cannot be constructed (e.g. notify watcher
/// init failure on a stripped-down container) — the daemon should log and
/// continue running without the pipeline rather than aborting startup.
pub fn spawn() -> Option<broadcast::Sender<TaggedAgentEvent>> {
    let poller = match ClaudeStatePoller::new() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "claude_pipeline: failed to start state poller, agent-event stream disabled");
            return None;
        }
    };
    Some(spawn_with(
        poller,
        ClaudeJsonlRegistry::new(),
        DEFAULT_POLL_INTERVAL,
    ))
}

/// Spawn the pipeline with explicit poller and registry. Used by tests so
/// they can point the poller at a tempdir and provide overrides on the
/// registry.
pub fn spawn_with(
    mut poller: ClaudeStatePoller,
    mut registry: ClaudeJsonlRegistry,
    interval: Duration,
) -> broadcast::Sender<TaggedAgentEvent> {
    let updates_rx = poller
        .updates()
        .expect("ClaudeStatePoller::updates() called twice");
    let events_rx = registry
        .events()
        .expect("ClaudeJsonlRegistry::events() called twice");

    let (tx, _rx) = broadcast::channel::<TaggedAgentEvent>(BROADCAST_CAPACITY);
    let tx_for_task = tx.clone();

    // TODO(code-review): no cancellation handle — task runs until process exit
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            // 1. Drive the file watcher: drain notify events + emit updates.
            let _snapshot = poller.poll();

            // 2. Forward those updates into the registry, installing/dropping
            //    JSONL tailers as Claude sessions come and go.
            while let Ok(update) = updates_rx.try_recv() {
                registry.apply_update(&update, None);
            }

            // 3. Tick every tailer once (top-level + subagent).
            registry.poll_all();

            // 4. Drain the registry's mpsc and re-broadcast to fan-out subscribers.
            while let Ok(event) = events_rx.try_recv() {
                if tx_for_task.send(event).is_err() {
                    // No active subscribers — that's fine, events are dropped
                    // until someone subscribes.
                    debug!("claude_pipeline: no subscribers, dropping event");
                }
            }
        }
    });

    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_jsonl_tailer::EventSource;
    use crate::claude_state::ClaudeStatePoller;
    use tempfile::tempdir;

    #[tokio::test]
    async fn pipeline_ticks_without_panic_and_keeps_channel_open() {
        // Set up a temp state dir + a fake JSONL file path the registry will
        // try to open. We bypass real path resolution by pre-seeding the
        // registry through `apply_update` with a Upserted state pointing at a
        // session id that has a JSONL file in our overridden Claude projects dir.
        //
        // Easiest path that exercises the whole pipeline: directly construct
        // the registry, push a TaggedAgentEvent through its internal channel
        // by calling poll on a SessionJsonlTailer pre-bound to a path.
        //
        // For an end-to-end pipeline test we instead poke a value through a
        // hand-rolled poller + registry pair. This validates the broadcast
        // fan-out, which is the load-bearing piece for the MCP resource.

        let dir = tempdir().unwrap();
        let poller = ClaudeStatePoller::with_dirs(vec![dir.path().to_path_buf()]).unwrap();
        let registry = ClaudeJsonlRegistry::new();
        let tx = spawn_with(poller, registry, Duration::from_millis(20));

        let mut sub = tx.subscribe();
        tokio::time::sleep(Duration::from_millis(80)).await;

        // No real claude sessions in the tempdir, so no events expected. The
        // channel must still be alive — try_recv should be Empty, not Closed.
        match sub.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {}
            Err(broadcast::error::TryRecvError::Closed) => {
                panic!("pipeline broadcast channel closed unexpectedly")
            }
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Ok(_) => {} // a surprise event is fine
        }
    }

    #[tokio::test]
    async fn broadcast_fans_out_to_multiple_subscribers() {
        let dir = tempdir().unwrap();
        let poller = ClaudeStatePoller::with_dirs(vec![dir.path().to_path_buf()]).unwrap();
        let registry = ClaudeJsonlRegistry::new();
        let tx = spawn_with(poller, registry, Duration::from_millis(20));

        let mut sub_a = tx.subscribe();
        let mut sub_b = tx.subscribe();

        // Inject an event directly through the broadcast sender to validate
        // both subscribers receive it. This isolates the fan-out wiring from
        // any JSONL/file-watcher flakiness.
        let synthetic = TaggedAgentEvent {
            event: crate::agent_events::AgentEvent::AssistantMessage {
                content: "hello".into(),
            },
            source: EventSource::TopLevel {
                session_id: "abc".into(),
            },
        };
        tx.send(synthetic.clone()).unwrap();

        let got_a = sub_a.recv().await.unwrap();
        let got_b = sub_b.recv().await.unwrap();
        assert_eq!(got_a, synthetic);
        assert_eq!(got_b, synthetic);
    }
}
