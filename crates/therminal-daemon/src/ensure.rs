//! `ensure_daemon()` -- the primary entry point for daemon lifecycle management.
//!
//! Called by the terminal app (or CLI) to guarantee a daemon is running with
//! a compatible protocol version. Handles three cases:
//!
//! 1. **No daemon running**: start a new one.
//! 2. **Daemon running, matching protocol version**: reuse it.
//! 3. **Daemon running, different protocol version**: graceful handoff.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use therminal_protocol::DaemonState;

use crate::client;
use crate::handoff::{self, DaemonCheck};
use crate::ipc_transport::{cleanup_socket, socket_exists};
use crate::lifecycle::{Lifecycle, LifecycleConfig};
use crate::mcp;
use crate::server::DaemonServer;

/// Build hash embedded at compile time by `build.rs`.
pub const BUILD_HASH: &str = env!("BUILD_HASH");

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Result of `ensure_daemon()`.
pub enum EnsureResult {
    /// An existing daemon is already running with a matching build.
    Reused,
    /// A new daemon was started (either fresh or via handoff).
    Started {
        /// Handle to the running daemon lifecycle.
        lifecycle: Arc<Lifecycle>,
    },
}

/// Ensure a daemon is running with a compatible protocol version.
///
/// This is the main entry point for daemon lifecycle management. It:
/// 1. Checks if a daemon is already running via socket probe.
/// 2. If running with matching protocol version, returns `Reused`.
/// 3. If running with different protocol version, performs graceful handoff.
/// 4. If no daemon, starts a new one.
///
/// Returns an `EnsureResult` indicating what happened.
pub async fn ensure_daemon(config: LifecycleConfig) -> Result<EnsureResult> {
    let socket_path = therminal_runtime::paths::socket_path("daemon");

    info!(
        protocol_version = therminal_protocol::PROTOCOL_VERSION,
        build_hash = BUILD_HASH,
        version = VERSION,
        socket = %socket_path.display(),
        "ensure_daemon called"
    );

    // Ensure runtime directory exists
    therminal_runtime::paths::ensure_runtime_dir().context("failed to create runtime directory")?;

    // On Unix, the handoff may return received FDs to restore into the new daemon.
    #[cfg(unix)]
    let mut received_handoff: Option<handoff::ReceivedHandoff> = None;

    // Check existing daemon -- handoff is based on protocol version, not build hash
    match handoff::check_daemon(&socket_path, therminal_protocol::PROTOCOL_VERSION).await {
        DaemonCheck::Reuse => {
            info!("reusing existing daemon");
            return Ok(EnsureResult::Reused);
        }
        DaemonCheck::NeedsHandoff { old_build_hash } => {
            info!(
                old_build_hash = %old_build_hash,
                new_build_hash = BUILD_HASH,
                protocol_version = therminal_protocol::PROTOCOL_VERSION,
                "performing protocol version handoff"
            );
            #[cfg(unix)]
            {
                received_handoff = handoff::perform_handoff(&socket_path).await?;
            }
            #[cfg(not(unix))]
            {
                handoff::perform_handoff(&socket_path).await?;
            }
        }
        DaemonCheck::IncompatibleDaemon => {
            warn!(
                "incompatible daemon detected on socket \
                 -- attempting graceful shutdown before starting fresh"
            );
            // Try to shut down whatever is listening, even though it
            // may not understand our protocol.  If it fails, force-remove.
            match client::request_shutdown(&socket_path).await {
                Ok(_) => {
                    info!("incompatible daemon acknowledged shutdown, waiting for socket removal");
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
                    loop {
                        if !socket_exists(&socket_path) {
                            break;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            warn!(
                                "incompatible daemon did not release socket in time, force-removing"
                            );
                            cleanup_socket(&socket_path);
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "failed to send shutdown to incompatible daemon, force-removing socket"
                    );
                    cleanup_socket(&socket_path);
                }
            }
        }
        DaemonCheck::StartFresh => {
            // Clean any stale socket (Unix; no-op on Windows where pipes
            // are not filesystem entries)
            if socket_exists(&socket_path) {
                warn!(path = %socket_path.display(), "removing stale socket");
                cleanup_socket(&socket_path);
            }
        }
    }

    // Start new daemon
    #[cfg(unix)]
    let lifecycle = start_daemon(socket_path, config, received_handoff).await?;
    #[cfg(not(unix))]
    let lifecycle = start_daemon(socket_path, config).await?;

    Ok(EnsureResult::Started { lifecycle })
}

/// Start a new daemon instance, binding the control socket and entering
/// the accept loop.
///
/// On Unix, if `received_handoff` is `Some`, restores sessions from the
/// received PTY master FDs before entering the accept loop.
async fn start_daemon(
    socket_path: PathBuf,
    config: LifecycleConfig,
    #[cfg(unix)] mut received_handoff: Option<handoff::ReceivedHandoff>,
) -> Result<Arc<Lifecycle>> {
    let lifecycle = Arc::new(Lifecycle::new(config));

    // Starting -> Binding
    lifecycle.transition(DaemonState::Binding)?;

    let mut server = DaemonServer::bind(
        socket_path,
        Arc::clone(&lifecycle),
        BUILD_HASH.to_string(),
        VERSION.to_string(),
    )
    .await
    .context("failed to bind daemon socket")?;

    // Restore sessions from handoff FDs before transitioning to Ready.
    #[cfg(unix)]
    if let Some(ref mut handoff) = received_handoff
        && let Some(fds) = handoff.take_fds()
    {
        let session_mgr = server.session_manager();
        let mut mgr = session_mgr.lock().await;
        let restored = mgr.restore_from_handoff(&handoff.payload, fds);
        lifecycle.set_session_count(mgr.session_count());
        info!(
            restored_panes = restored,
            "sessions restored from handoff FDs"
        );
    }

    // If no sessions were restored from handoff, try loading persisted state.
    {
        let session_mgr = server.session_manager();
        let mgr = session_mgr.lock().await;
        let has_sessions = mgr.session_count() > 0;
        drop(mgr);

        if !has_sessions && let Some(persisted) = crate::persistence::load() {
            let mut mgr = session_mgr.lock().await;
            let restored = mgr.restore_from_persisted(&persisted);
            lifecycle.set_session_count(mgr.session_count());
            if restored > 0 {
                info!(
                    restored_panes = restored,
                    "sessions restored from persisted state"
                );
            }
        }
    }

    // Spawn the debounced persistence task.
    let persist_shutdown = lifecycle.shutdown_notify();
    let (persist_handle, _persist_task) =
        crate::persistence::spawn_persistence_task(server.session_manager(), persist_shutdown);
    {
        let session_mgr = server.session_manager();
        let mut mgr = session_mgr.lock().await;
        mgr.set_persistence(persist_handle);
    }

    // Binding -> Ready
    lifecycle.transition(DaemonState::Ready)?;

    // Ready -> Running
    lifecycle.transition(DaemonState::Running)?;

    // Spawn the idle watcher
    lifecycle.spawn_idle_watcher();

    // Load trust config for MCP enforcement.
    let app_config = therminal_core::config::TherminalConfig::load();
    let trust_config = Arc::new(app_config.trust.clone());
    let rate_limiter = Arc::new(crate::trust::RateLimiter::new(
        app_config.trust.destructive_rate_limit,
    ));

    // Build the shared OSC handler registry (tn-hkpz) and activate
    // harness-crate claims BEFORE any PTY is opened. Harness crates
    // register their OSC codes once here; the registry Arc is then cloned
    // into every pane's `TherminalInterceptor` via `SessionManager::
    // set_osc_registry`. A duplicate claim (two crates fighting for the
    // same code) is a programming mistake and should fail the daemon
    // startup — `.expect()` makes the failure loud.
    //
    // See `docs/osc-handler-registry.md` for the registration API and
    // `docs/osc-code-registry.md` for the canonical code table.
    let osc_registry = Arc::new(therminal_terminal::OscHandlerRegistry::new());
    therminal_harness_claude::activate_markers(&osc_registry)
        .expect("claude OSC marker handler failed to register — check docs/osc-code-registry.md");
    // tn-gln6 #1: wire the harness-event sink BEFORE any session is
    // created. Panes constructed after this call will clone the sender
    // into their `TherminalInterceptor` so OSC 1341 marker events
    // (and any future harness OSC events) reach a daemon-side consumer
    // instead of being silently dropped by the registry dispatch.
    //
    // The channel is a `std::sync::mpsc` because the producer runs on the
    // PTY reader thread (not a tokio task). A dedicated OS thread drains
    // it; today we log at `debug` and broadcast nothing further, but this
    // is the hook point where a future `terminal://events` bus (tn-xula)
    // will consume harness events without re-touching the pane plumbing.
    let (harness_event_tx, harness_event_rx) =
        std::sync::mpsc::channel::<therminal_terminal::TaggedHarnessEvent>();
    {
        let session_mgr = server.session_manager();
        let mut mgr = session_mgr.lock().await;
        mgr.set_osc_registry(Arc::clone(&osc_registry));
        mgr.set_harness_event_sink(harness_event_tx);
    }
    // We construct the event bus a few lines below, but the drain thread
    // needs a clone of it. Build a one-element holder via Arc::new and pass
    // it down. To keep the diff localized, the bus is constructed here,
    // earlier than its previous position; the MCP-side `event_bus_for_mcp`
    // holder still picks it up via Arc clone below.
    let early_event_bus = std::sync::Arc::new(crate::event_bus::EventBus::with_default_capacity());
    let bus_for_drain = std::sync::Arc::clone(&early_event_bus);
    // Drain the harness-event channel on a dedicated OS thread. Using a
    // plain thread (not tokio::spawn_blocking) keeps the daemon working
    // when the tokio runtime is paused, and avoids warning-storms from
    // blocking recv on a runtime worker. The drain doubles as a bridge
    // into the unified event bus (tn-xula): every TaggedHarnessEvent
    // becomes a `TerminalEvent { source_class: harness, source_id: <owner>,
    // kind: <handler-supplied>, body: <handler-supplied> }`.
    std::thread::Builder::new()
        .name("harness-event-drain".into())
        .spawn(move || {
            while let Ok(tagged) = harness_event_rx.recv() {
                debug!(
                    source_id = tagged.source_id,
                    kind = %tagged.event.kind,
                    "harness OSC event received"
                );
                bus_for_drain.publish(therminal_protocol::bus_types::TerminalEvent {
                    source_class: therminal_protocol::bus_types::SourceClass::Harness,
                    source_id: tagged.source_id.to_string(),
                    kind: tagged.event.kind,
                    pane_id: None,
                    ts_ms: 0,
                    cursor: 0,
                    body: tagged.event.body,
                });
            }
            debug!("harness-event-drain: channel closed, exiting");
        })
        .expect("failed to spawn harness-event drain thread");
    info!(
        code = therminal_harness_claude::CLAUDE_OSC_CODE,
        owner = therminal_harness_claude::CLAUDE_OWNER,
        "OSC handler registry active; claude markers installed"
    );

    // Spawn the Claude agent-event pipeline (file watcher → JSONL tailers →
    // broadcast). Returns None if the OS file watcher cannot be created, in
    // which case the MCP `therminal://claude/events` resource will simply
    // produce zero events.
    // Bridge channel: the pipeline's sync observer pushes ClaudeStateUpdates
    // here, and a dedicated tokio task drains them and writes the per-pane
    // capacity cache (resolving pane_id via the AgentRegistry under the
    // session-manager mutex). We must not block the pipeline tick on the
    // tokio mutex, hence the channel hop.
    let (capacity_update_tx, mut capacity_update_rx) = tokio::sync::mpsc::unbounded_channel::<
        therminal_harness_claude::state::ClaudeStateUpdate,
    >();
    let capacity_observer: therminal_harness_claude::pipeline::StateUpdateObserver =
        Arc::new(move |update| {
            let _ = capacity_update_tx.send(update.clone());
        });

    {
        let session_mgr = server.session_manager();
        let cache = {
            let mgr = session_mgr.lock().await;
            mgr.pane_capacity_cache()
        };
        let session_mgr_for_task = Arc::clone(&session_mgr);
        tokio::spawn(async move {
            while let Some(update) = capacity_update_rx.recv().await {
                match update {
                    therminal_harness_claude::state::ClaudeStateUpdate::Upserted(state) => {
                        let mgr = session_mgr_for_task.lock().await;
                        let pane_id = crate::pane_capacity::resolve_pane_id_from_state(
                            &state,
                            mgr.agent_registry(),
                        );
                        // tn-ixfy: fallback when PID matching fails (common on
                        // Windows+WSL where the state file PID from hook scripts
                        // diverges from the process detector's WSL-probe PID).
                        // Match by working_dir against panes with a Claude agent.
                        let pane_id = pane_id.or_else(|| {
                            let wd = state.working_dir.as_deref().filter(|s| !s.is_empty())?;
                            for entry in mgr.agent_registry().agents() {
                                if matches!(
                                    entry.agent_type,
                                    therminal_terminal::state_inference::AgentType::Claude
                                ) && let Some(cwd) = mgr.pane_cwd(entry.pane_id)
                                    && cwd == wd
                                {
                                    tracing::debug!(
                                        pane_id = entry.pane_id,
                                        working_dir = wd,
                                        "pane_capacity: PID miss, matched by cwd"
                                    );
                                    return Some(entry.pane_id);
                                }
                            }
                            None
                        });
                        drop(mgr);
                        if let Some(pid) = pane_id {
                            cache.upsert(pid, crate::pane_capacity::entry_from_state(&state));
                        }
                    }
                    therminal_harness_claude::state::ClaudeStateUpdate::Removed { path: _ } => {
                        // The poller does not surface session_id on removal, only
                        // the path. Best-effort: derive session_id from the file
                        // stem and let the cache drop any matching entry.
                        // (Path-stem vs session_id is not always 1:1, so this is
                        // a no-op when they differ — entries also age out via the
                        // TTL sweep below.)
                    }
                }

                // tn-hxso: sweep stale entries on every update tick.
                let evicted = cache.evict_stale(crate::pane_capacity::DEFAULT_STALE_TTL_SECS);
                if evicted > 0 {
                    tracing::debug!(evicted, "pane_capacity: evicted stale entries");
                }
            }
        });
    }

    let claude_harness = therminal_harness_claude::ClaudeHarness::start(
        lifecycle.shutdown_notify(),
        Some(capacity_observer),
    );
    // Wire the hook-push sink so IpcRequest::PushAgentEvent signals from WSL
    // hook scripts are forwarded to the harness broadcast channel. When the
    // harness is disabled (notify watcher init failure), the sink is None and
    // PushAgentEvent returns IpcResponse::Error with a clear message.
    if let Some(events_tx) = claude_harness.event_stream() {
        let sink = therminal_harness_claude::HookPushSink::new(events_tx.clone());
        server.set_hook_push_sink(sink);
    }
    let claude_events_tx = claude_harness.into_event_stream();

    // Spawn the daemon-side process-tree agent detector ticker (tn-pehl).
    // Walks each pane's shell PID every 3 seconds and feeds detected
    // agents into the central `AgentRegistry`. Without this, the daemon's
    // `terminal.agents.list` MCP tool returns `[]` for any session that
    // wasn't created by an attached GUI — see process_detector_task.rs.
    let _process_detector_task = crate::process_detector_task::spawn_process_detector_task(
        server.session_manager(),
        lifecycle.shutdown_notify(),
    );

    // Wire the AgentRegistry's lifecycle events into a tokio broadcast channel
    // for the MCP `therminal://agents/events` resource. The registry takes a
    // type-erased callback so therminal-terminal stays free of a tokio dep.
    let (agent_events_tx, _) = tokio::sync::broadcast::channel::<
        therminal_terminal::agent_registry::TaggedAgentEvent,
    >(256);
    {
        let tx = agent_events_tx.clone();
        let session_mgr = server.session_manager();
        let mut mgr = session_mgr.lock().await;
        mgr.set_agent_event_broadcaster(Arc::new(move |evt| {
            // Drop on no subscribers — broadcast::send returns Err which we ignore.
            let _ = tx.send(evt);
        }));
    }
    let agent_events_tx_for_mcp = Some(agent_events_tx);

    // Build the semantic pattern engine (tn-yrjd) from the loaded
    // `[patterns]` config. Every field declared on `PatternsConfig` is
    // mapped here — that's the "no dead config" discipline from the
    // project CLAUDE.md. The engine loads user packs from
    // `app_config.patterns.directory` (or `<config_dir>/patterns` when
    // unset) plus shipped example packs resolved via
    // `THERMINAL_RESOURCES_DIR` / runtime layout.
    let pattern_engine = Arc::new(therminal_terminal::semantic_patterns::PatternEngine::new(
        therminal_terminal::semantic_patterns::PatternEngineConfig {
            enabled: app_config.patterns.enabled,
            user_pattern_dir: app_config.patterns.directory.clone(),
            shipped_pattern_dir: None,
            max_patterns: app_config.patterns.max_patterns,
            slow_pattern_threshold_us: app_config.patterns.slow_pattern_threshold_us,
            slow_strike_limit: app_config.patterns.slow_strike_limit,
        },
    ));
    let pattern_engine_for_mcp = Some(Arc::clone(&pattern_engine));

    // tn-86us: install the engine + event bus on the session manager so
    // every pane's `DaemonPtyHandler` gets a dispatcher scoped to the same
    // shared state. This runs AFTER handoff / persisted-state restore —
    // restored panes pre-date the dispatcher install and therefore do not
    // get pattern dispatch until they are replaced. Any session created
    // by `CreateSession` or `SplitPane` after this point picks it up.
    {
        let session_mgr = server.session_manager();
        let mut mgr = session_mgr.lock().await;
        mgr.set_pattern_dispatch(
            Arc::clone(&pattern_engine),
            std::sync::Arc::clone(&early_event_bus),
        );
    }

    // Hot-reload pattern packs on filesystem change (tn-yrjd). Uses the
    // same `notify` crate the `therminal.toml` watcher runs under, but
    // lives entirely in the daemon — the user pattern directory is a
    // daemon concern, not a GUI concern. We watch the resolved user
    // pattern directory (the configured override or the default
    // `<config_dir>/patterns`), ignore read errors, and spawn a tiny
    // tokio task that debounces events by 500ms and calls
    // `engine.reload()` on the next quiet moment. Missing directories
    // are not an error — users who have never touched packs still get a
    // working engine.
    let pattern_watch_dir = app_config
        .patterns
        .directory
        .clone()
        .unwrap_or_else(|| therminal_runtime::paths::config_dir().join("patterns"));
    // tn-gln6 #2: create the pattern directory eagerly so the notify watcher
    // can be installed even on first-time installs. Previously the watcher
    // was only started if the directory already existed — a user who
    // created the dir and dropped in their first pack after the daemon was
    // already running got no hot-reload, ever. Creating up front also
    // documents the expected location for users browsing their config dir.
    if let Err(e) = std::fs::create_dir_all(&pattern_watch_dir) {
        warn!(
            path = %pattern_watch_dir.display(),
            error = %e,
            "failed to create pattern pack directory — hot-reload disabled"
        );
    }
    if pattern_watch_dir.exists() {
        let engine_for_watch = Arc::clone(&pattern_engine);
        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        // Keep the watcher alive for the lifetime of the daemon by leaking
        // it into a static — the alternative is to stash the handle on the
        // Lifecycle which is more plumbing than this one use case merits.
        // `notify::recommended_watcher` returns a `Box<dyn Watcher>` held
        // by the spawned task.
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                use notify::EventKind;
                if matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    let _ = notify_tx.send(());
                }
            }
        }) {
            Ok(mut watcher) => {
                if let Err(e) = notify::Watcher::watch(
                    &mut watcher,
                    &pattern_watch_dir,
                    notify::RecursiveMode::NonRecursive,
                ) {
                    warn!(
                        path = %pattern_watch_dir.display(),
                        error = %e,
                        "failed to start pattern pack watcher"
                    );
                } else {
                    info!(
                        path = %pattern_watch_dir.display(),
                        "pattern pack watcher started"
                    );
                    tokio::spawn(async move {
                        // Move the watcher into the task so it stays alive
                        // for the duration of the daemon. Dropping it stops
                        // the watch.
                        let _watcher = watcher;
                        loop {
                            // Wait for the first event.
                            if notify_rx.recv().await.is_none() {
                                break;
                            }
                            // Debounce: drain any events that arrive within
                            // 500ms before reloading.
                            loop {
                                match tokio::time::timeout(
                                    std::time::Duration::from_millis(500),
                                    notify_rx.recv(),
                                )
                                .await
                                {
                                    Ok(Some(())) => continue,
                                    Ok(None) => return,
                                    Err(_) => break,
                                }
                            }
                            info!("pattern pack watcher: reloading");
                            engine_for_watch.reload();
                        }
                    });
                }
            }
            Err(e) => warn!(
                path = %pattern_watch_dir.display(),
                error = %e,
                "failed to construct pattern pack watcher — hot-reload disabled"
            ),
        }
    }

    // Unified event bus (tn-xula). Constructed earlier so the OSC drain
    // thread can hold an Arc clone; the MCP server gets another clone here.
    let event_bus = Arc::clone(&early_event_bus);
    let event_bus_for_mcp = Some(Arc::clone(&event_bus));

    // Bridge: claude harness broadcast → unified bus. The harness emits
    // `TaggedAgentEvent`s; we wrap each one as a `TerminalEvent` with
    // `source_class=harness, source_id="claude"`. Pane resolution is
    // best-effort — we leave `pane_id=None` because the harness's event
    // shape carries `EventSource` (TopLevel/Subagent) rather than a pane id.
    if let Some(claude_tx) = claude_events_tx.as_ref() {
        let mut rx = claude_tx.subscribe();
        let bus = Arc::clone(&event_bus);
        tokio::spawn(async move {
            tracing::info!("harness->bus bridge started (claude)");
            loop {
                match rx.recv().await {
                    Ok(tagged) => {
                        let body = serde_json::to_value(&tagged)
                            .unwrap_or_else(|_| serde_json::json!({ "serialize_error": true }));
                        bus.publish(therminal_protocol::bus_types::TerminalEvent {
                            source_class: therminal_protocol::bus_types::SourceClass::Harness,
                            source_id: "claude".to_string(),
                            kind: "claude.event".to_string(),
                            pane_id: None,
                            ts_ms: 0,
                            cursor: 0,
                            body,
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::error!(
                            "harness->bus bridge (claude) exiting: source channel closed"
                        );
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("harness->bus bridge lagged, skipped {} events", n);
                        bus.note_dropped_subscriber();
                        continue;
                    }
                }
            }
        });
    }

    // Spawn the MCP server alongside the IPC server. Note: the harness OSC
    // event drain thread above (`harness-event-drain`) is a sync mpsc loop;
    // republishing into the bus from there happens via the dedicated bridge
    // task installed below to avoid synchronously holding the bus mutex from
    // the PTY reader thread.

    // Start MCP server alongside the IPC server
    let mcp_shutdown = Arc::new(tokio::sync::Notify::new());
    let mcp_config = app_config.mcp.clone();
    let mcp_session_mgr = server.session_manager();
    let mcp_shutdown_clone = Arc::clone(&mcp_shutdown);
    let mcp_trust = Arc::clone(&trust_config);
    let mcp_rl = Arc::clone(&rate_limiter);
    tokio::spawn(async move {
        if let Err(e) = mcp::start_mcp_server(
            mcp_config,
            mcp_session_mgr,
            mcp_trust,
            mcp_rl,
            claude_events_tx,
            agent_events_tx_for_mcp,
            pattern_engine_for_mcp,
            event_bus_for_mcp,
            mcp_shutdown_clone,
        )
        .await
        {
            warn!(error = %e, "MCP server exited with error");
        }
    });

    // Spawn the server accept loop in the background
    let lc = Arc::clone(&lifecycle);
    tokio::spawn(async move {
        if let Err(e) = server.run().await {
            warn!(error = %e, "daemon server exited with error");
        }
        // Signal MCP server to stop when IPC server stops
        mcp_shutdown.notify_one();
        // Ensure lifecycle reaches Stopped
        if lc.state() != DaemonState::Stopped {
            let _ = lc.initiate_shutdown().await;
        }
    });

    info!(
        build_hash = BUILD_HASH,
        version = VERSION,
        "daemon started successfully"
    );

    Ok(lifecycle)
}
