//! Unified terminal event bus (tn-xula).
//!
//! Implements `docs/event-bus-spec.md`. The bus is a single in-memory ring
//! buffer of [`TerminalEvent`]s with a monotonic cursor, plus a tokio
//! broadcast channel for live notification of subscribers. All three
//! integration surfaces (harness crates, pattern packs, core capabilities)
//! publish through [`EventBus::publish`]; subscribers consume via the MCP
//! resource `terminal://events?<filters>`.
//!
//! ## Backpressure
//!
//! Live subscribers are wired through `tokio::sync::broadcast` whose channel
//! capacity equals the configured ring capacity. A subscriber that lags more
//! than [`MAX_SUBSCRIBER_LAG`] events is dropped silently with a warn log,
//! per SPEC §5.
//!
//! ## Body size cap
//!
//! Publish enforces the 64 KB hard cap from SPEC §1. An over-cap event is
//! replaced with a synthetic `core` / `bus.body_too_large` event whose body
//! records the offending source and original byte length.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;
use tracing::warn;

use therminal_protocol::bus_types::{SourceClass, TerminalEvent};

/// Default ring buffer capacity (number of events).
pub const DEFAULT_RING_CAPACITY: usize = 10_000;

/// Hard cap on the serialized body size of a single event, in bytes.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Subscribers lagging more than this many events are dropped.
pub const MAX_SUBSCRIBER_LAG: u64 = 1_000;

/// Maximum events returned per `query` page (also used by the MCP resource).
pub const MAX_PAGE_SIZE: usize = 500;

// ── Filter ──────────────────────────────────────────────────────────────

/// Compiled filter for `terminal://events?<query>`.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub source_class: Option<SourceClass>,
    pub source_id: Option<String>,
    /// Glob patterns for `kind`. Empty = all kinds.
    pub kinds: Vec<String>,
    /// Pane ids; when non-empty, events with `pane_id = None` are excluded.
    pub panes: Vec<u64>,
    /// Cursor lower bound (exclusive). `0` = from oldest available.
    pub since: u64,
}

impl EventFilter {
    /// Parse a filter from a `terminal://events?...` URI's query string.
    ///
    /// The argument is just the part after `?` (no leading `?`); pass an
    /// empty string for an unfiltered subscription.
    pub fn from_query(query: &str) -> Result<Self, String> {
        let mut out = EventFilter::default();
        if query.is_empty() {
            return Ok(out);
        }
        for part in query.split('&') {
            if part.is_empty() {
                continue;
            }
            let (key, val) = match part.split_once('=') {
                Some(kv) => kv,
                None => return Err(format!("malformed filter param: {part}")),
            };
            match key {
                "source_class" => {
                    out.source_class = Some(match val {
                        "harness" => SourceClass::Harness,
                        "pattern" => SourceClass::Pattern,
                        "core" => SourceClass::Core,
                        other => return Err(format!("unknown source_class: {other}")),
                    });
                }
                "source_id" => out.source_id = Some(val.to_string()),
                "kinds" => {
                    out.kinds = val
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect();
                }
                "panes" => {
                    for tok in val.split(',') {
                        let id: u64 = tok
                            .parse()
                            .map_err(|e| format!("invalid pane id {tok}: {e}"))?;
                        out.panes.push(id);
                    }
                }
                "since" => {
                    out.since = val
                        .parse()
                        .map_err(|e| format!("invalid since cursor {val}: {e}"))?;
                }
                other => return Err(format!("unknown filter param: {other}")),
            }
        }
        Ok(out)
    }

    /// Test whether `event` matches this filter (ignoring `since`).
    pub fn matches(&self, event: &TerminalEvent) -> bool {
        if let Some(sc) = self.source_class
            && event.source_class != sc
        {
            return false;
        }
        if let Some(ref sid) = self.source_id
            && &event.source_id != sid
        {
            return false;
        }
        if !self.panes.is_empty() {
            let Some(pid) = event.pane_id else {
                return false;
            };
            if !self.panes.contains(&pid) {
                return false;
            }
        }
        if !self.kinds.is_empty() {
            let mut any = false;
            for pat in &self.kinds {
                if glob_match(pat, &event.kind) {
                    any = true;
                    break;
                }
            }
            if !any {
                return false;
            }
        }
        true
    }
}

/// Simple `*`-only glob match: `*` matches any sequence (including dots and
/// the empty string). No `?`, no character classes, no escaping.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Iterative DP — small inputs (kind strings are < 64 bytes), so a
    // straightforward two-pointer with backtracking is fine.
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_t) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            star_t = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ── Stats ───────────────────────────────────────────────────────────────

/// Snapshot for `terminal.events.stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBusStats {
    pub total_events: u64,
    pub events_harness: u64,
    pub events_pattern: u64,
    pub events_core: u64,
    pub dropped_subscribers: u64,
    pub buffer_used: usize,
    pub buffer_capacity: usize,
    pub buffer_fill_pct: f32,
    pub current_cursor: u64,
}

// ── Bus ─────────────────────────────────────────────────────────────────

struct BusInner {
    ring: VecDeque<TerminalEvent>,
    capacity: usize,
}

/// The unified event bus.
///
/// Cloneable via `Arc<EventBus>` — only one bus exists per daemon. Construct
/// once at startup in `ensure_daemon` and hand `Arc` clones to publishers and
/// the MCP server.
pub struct EventBus {
    inner: Mutex<BusInner>,
    cursor: AtomicU64,
    total_events: AtomicU64,
    events_harness: AtomicU64,
    events_pattern: AtomicU64,
    events_core: AtomicU64,
    dropped_subscribers: AtomicU64,
    notify: broadcast::Sender<TerminalEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(16);
        // Channel capacity = ring capacity so a subscriber that's keeping up
        // never sees Lagged. Subscribers that fall behind get dropped at the
        // forwarder level; see `subscribe`.
        let (notify, _) = broadcast::channel(cap);
        Self {
            inner: Mutex::new(BusInner {
                ring: VecDeque::with_capacity(cap),
                capacity: cap,
            }),
            cursor: AtomicU64::new(0),
            total_events: AtomicU64::new(0),
            events_harness: AtomicU64::new(0),
            events_pattern: AtomicU64::new(0),
            events_core: AtomicU64::new(0),
            dropped_subscribers: AtomicU64::new(0),
            notify,
        }
    }

    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_RING_CAPACITY)
    }

    /// Publish an event. The bus assigns the cursor and `ts_ms` if the
    /// caller left them at zero, then appends to the ring and notifies
    /// live subscribers.
    ///
    /// Enforces the 64 KB hard body cap by replacing the body with a
    /// `bus.body_too_large` core event in place.
    pub fn publish(&self, mut event: TerminalEvent) -> u64 {
        if event.ts_ms == 0 {
            event.ts_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
        }

        // Body cap enforcement.
        if let Ok(serialized) = serde_json::to_vec(&event.body)
            && serialized.len() > MAX_BODY_BYTES
        {
            warn!(
                source_id = %event.source_id,
                kind = %event.kind,
                body_bytes = serialized.len(),
                "event-bus: body exceeds 64KB cap, replacing with bus.body_too_large"
            );
            event = TerminalEvent {
                source_class: SourceClass::Core,
                source_id: "event-bus".to_string(),
                kind: "bus.body_too_large".to_string(),
                pane_id: event.pane_id,
                ts_ms: event.ts_ms,
                cursor: 0,
                body: json!({
                    "original_source_class": event.source_class,
                    "original_source_id": event.source_id,
                    "original_kind": event.kind,
                    "body_bytes": serialized.len(),
                }),
            };
        }

        let cursor = self.cursor.fetch_add(1, Ordering::Relaxed) + 1;
        event.cursor = cursor;

        match event.source_class {
            SourceClass::Harness => {
                self.events_harness.fetch_add(1, Ordering::Relaxed);
            }
            SourceClass::Pattern => {
                self.events_pattern.fetch_add(1, Ordering::Relaxed);
            }
            SourceClass::Core => {
                self.events_core.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.total_events.fetch_add(1, Ordering::Relaxed);

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.ring.len() == inner.capacity {
                inner.ring.pop_front();
            }
            inner.ring.push_back(event.clone());
        }

        // Drop on no subscribers — broadcast::send returns Err which we
        // intentionally ignore.
        let _ = self.notify.send(event);
        cursor
    }

    /// Convenience publisher for the core hotspot-click event.
    pub fn publish_core_hotspot_click(&self, pane_id: u64, uri: &str, kind: &str) -> u64 {
        self.publish(TerminalEvent {
            source_class: SourceClass::Core,
            source_id: "core.hotspot".to_string(),
            kind: "clicked".to_string(),
            pane_id: Some(pane_id),
            ts_ms: 0,
            cursor: 0,
            body: json!({ "uri": uri, "kind": kind }),
        })
    }

    /// Pull-mode query. Returns up to `limit` events whose cursor is `>
    /// filter.since` and that match the rest of `filter`. The returned
    /// `next_cursor` is the cursor of the last returned event (or
    /// `filter.since` when nothing matched), and `has_more` is `true` when
    /// the page was capped.
    pub fn query(&self, filter: &EventFilter, limit: usize) -> QueryPage {
        let limit = limit.clamp(1, MAX_PAGE_SIZE);
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut events = Vec::new();
        let mut last_cursor = filter.since;
        let mut more = false;
        for ev in inner.ring.iter() {
            if ev.cursor <= filter.since {
                continue;
            }
            if !filter.matches(ev) {
                continue;
            }
            if events.len() == limit {
                more = true;
                break;
            }
            last_cursor = ev.cursor;
            events.push(ev.clone());
        }
        QueryPage {
            events,
            next_cursor: last_cursor,
            has_more: more,
        }
    }

    /// Push-mode subscription. Returns a [`broadcast::Receiver`] that
    /// observes every published event after this call. The MCP resource
    /// layer wraps this receiver in a per-connection forwarder that
    /// applies the filter and enforces the lag cap.
    pub fn subscribe(&self) -> broadcast::Receiver<TerminalEvent> {
        self.notify.subscribe()
    }

    /// Note that a subscriber was dropped due to backpressure.
    pub fn note_dropped_subscriber(&self) {
        self.dropped_subscribers.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stats(&self) -> EventBusStats {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let used = inner.ring.len();
        let cap = inner.capacity;
        let fill = if cap == 0 {
            0.0
        } else {
            (used as f32 / cap as f32) * 100.0
        };
        EventBusStats {
            total_events: self.total_events.load(Ordering::Relaxed),
            events_harness: self.events_harness.load(Ordering::Relaxed),
            events_pattern: self.events_pattern.load(Ordering::Relaxed),
            events_core: self.events_core.load(Ordering::Relaxed),
            dropped_subscribers: self.dropped_subscribers.load(Ordering::Relaxed),
            buffer_used: used,
            buffer_capacity: cap,
            buffer_fill_pct: fill,
            current_cursor: self.cursor.load(Ordering::Relaxed),
        }
    }

    /// Capacity of the underlying ring buffer.
    pub fn capacity(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capacity
    }
}

/// One page of results from [`EventBus::query`].
#[derive(Debug, Clone, Serialize)]
pub struct QueryPage {
    pub events: Vec<TerminalEvent>,
    pub next_cursor: u64,
    pub has_more: bool,
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(
        source_class: SourceClass,
        source_id: &str,
        kind: &str,
        pane: Option<u64>,
    ) -> TerminalEvent {
        TerminalEvent {
            source_class,
            source_id: source_id.to_string(),
            kind: kind.to_string(),
            pane_id: pane,
            ts_ms: 0,
            cursor: 0,
            body: json!({}),
        }
    }

    #[test]
    fn glob_basic() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("claude.*", "claude.thinking_started"));
        assert!(glob_match("*.error", "compile.error"));
        assert!(!glob_match("claude.*", "codex.tool_use"));
        assert!(glob_match("tool_call", "tool_call"));
        assert!(!glob_match("tool_call", "tool_call_v2"));
    }

    #[test]
    fn glob_empty_pattern_matches_empty_text() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn glob_empty_pattern_does_not_match_nonempty() {
        assert!(!glob_match("", "something"));
    }

    #[test]
    fn glob_star_matches_empty() {
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_double_star() {
        assert!(glob_match("**", "anything"));
        assert!(glob_match("a**b", "ab"));
        assert!(glob_match("a**b", "aXYZb"));
    }

    #[test]
    fn glob_star_in_middle() {
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "aXYZc"));
        assert!(!glob_match("a*c", "aXYZd"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("*.*.*", "a.b.c"));
        assert!(glob_match("*.*.*", "..."));
        assert!(!glob_match("*.*.*", "a.b"));
    }

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "EXACT"));
    }

    #[test]
    fn filter_query_empty_string_returns_default() {
        let f = EventFilter::from_query("").unwrap();
        assert!(f.source_class.is_none());
        assert!(f.source_id.is_none());
        assert!(f.kinds.is_empty());
        assert!(f.panes.is_empty());
        assert_eq!(f.since, 0);
    }

    #[test]
    fn filter_query_unknown_source_class_is_error() {
        let err = EventFilter::from_query("source_class=unknown").unwrap_err();
        assert!(err.contains("unknown source_class"));
    }

    #[test]
    fn filter_query_malformed_param_is_error() {
        let err = EventFilter::from_query("no_equals_sign").unwrap_err();
        assert!(err.contains("malformed"));
    }

    #[test]
    fn filter_query_invalid_pane_id_is_error() {
        let err = EventFilter::from_query("panes=abc").unwrap_err();
        assert!(err.contains("invalid pane id"));
    }

    #[test]
    fn filter_query_unknown_param_is_error() {
        let err = EventFilter::from_query("foo=bar").unwrap_err();
        assert!(err.contains("unknown filter param"));
    }

    #[test]
    fn filter_matches_kind_glob() {
        let f = EventFilter {
            kinds: vec!["claude.*".to_string()],
            ..Default::default()
        };
        assert!(f.matches(&ev(SourceClass::Harness, "claude", "claude.thinking", None)));
        assert!(!f.matches(&ev(SourceClass::Harness, "claude", "tool_call", None)));
    }

    #[test]
    fn filter_matches_source_id() {
        let f = EventFilter {
            source_id: Some("claude".to_string()),
            ..Default::default()
        };
        assert!(f.matches(&ev(SourceClass::Harness, "claude", "k", None)));
        assert!(!f.matches(&ev(SourceClass::Harness, "codex", "k", None)));
    }

    #[test]
    fn filter_query_parsing() {
        let f = EventFilter::from_query(
            "source_class=harness&source_id=claude&kinds=tool_call,*.error&panes=1,2&since=42",
        )
        .unwrap();
        assert!(matches!(f.source_class, Some(SourceClass::Harness)));
        assert_eq!(f.source_id.as_deref(), Some("claude"));
        assert_eq!(
            f.kinds,
            vec!["tool_call".to_string(), "*.error".to_string()]
        );
        assert_eq!(f.panes, vec![1, 2]);
        assert_eq!(f.since, 42);
    }

    #[test]
    fn publish_assigns_monotonic_cursor() {
        let bus = EventBus::with_default_capacity();
        let c1 = bus.publish(ev(SourceClass::Core, "test", "k", None));
        let c2 = bus.publish(ev(SourceClass::Core, "test", "k", None));
        assert_eq!(c1, 1);
        assert_eq!(c2, 2);
    }

    #[test]
    fn query_with_filter() {
        let bus = EventBus::with_default_capacity();
        bus.publish(ev(SourceClass::Harness, "claude", "tool_call", Some(1)));
        bus.publish(ev(SourceClass::Pattern, "cargo-errors", "error", Some(2)));
        bus.publish(ev(SourceClass::Harness, "codex", "tool_call", Some(3)));

        let filter = EventFilter::from_query("source_class=harness").unwrap();
        let page = bus.query(&filter, 100);
        assert_eq!(page.events.len(), 2);
        assert!(
            page.events
                .iter()
                .all(|e| matches!(e.source_class, SourceClass::Harness))
        );
    }

    #[test]
    fn cursor_resumption() {
        let bus = EventBus::with_default_capacity();
        for i in 0..100 {
            bus.publish(ev(SourceClass::Core, "t", &format!("k{i}"), None));
        }
        let filter = EventFilter {
            since: 50,
            ..Default::default()
        };
        let page = bus.query(&filter, 100);
        assert_eq!(page.events.len(), 50);
        assert_eq!(page.events.first().unwrap().cursor, 51);
        assert_eq!(page.events.last().unwrap().cursor, 100);
    }

    #[test]
    fn ring_evicts_oldest() {
        let bus = EventBus::new(16);
        for i in 0..50u64 {
            bus.publish(ev(SourceClass::Core, "t", &format!("k{i}"), None));
        }
        let page = bus.query(&EventFilter::default(), 500);
        assert_eq!(page.events.len(), 16);
        // Oldest in ring should be cursor 35 (50 - 16 + 1).
        assert_eq!(page.events.first().unwrap().cursor, 35);
        assert_eq!(page.events.last().unwrap().cursor, 50);
    }

    #[test]
    fn pane_filter_excludes_session_scoped() {
        let bus = EventBus::with_default_capacity();
        bus.publish(ev(SourceClass::Core, "t", "k", None));
        bus.publish(ev(SourceClass::Core, "t", "k", Some(7)));
        let filter = EventFilter {
            panes: vec![7],
            ..Default::default()
        };
        let page = bus.query(&filter, 100);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].pane_id, Some(7));
    }

    #[test]
    fn multi_source_filtering() {
        let bus = EventBus::with_default_capacity();
        bus.publish(ev(SourceClass::Harness, "claude", "tool_call", Some(1)));
        bus.publish(ev(SourceClass::Pattern, "cargo-errors", "error", Some(1)));
        bus.publish(ev(SourceClass::Core, "core.hotspot", "clicked", Some(1)));

        for class in [
            SourceClass::Harness,
            SourceClass::Pattern,
            SourceClass::Core,
        ] {
            let mut f = EventFilter::default();
            f.source_class = Some(class);
            assert_eq!(bus.query(&f, 100).events.len(), 1);
        }
    }

    #[test]
    fn body_size_cap_replaces_event() {
        let bus = EventBus::with_default_capacity();
        let huge = "x".repeat(MAX_BODY_BYTES + 1);
        bus.publish(TerminalEvent {
            source_class: SourceClass::Pattern,
            source_id: "huge".to_string(),
            kind: "boom".to_string(),
            pane_id: None,
            ts_ms: 0,
            cursor: 0,
            body: json!({ "data": huge }),
        });
        let page = bus.query(&EventFilter::default(), 10);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].kind, "bus.body_too_large");
        assert!(matches!(page.events[0].source_class, SourceClass::Core));
    }

    #[tokio::test]
    async fn subscribe_receives_published_events() {
        let bus = EventBus::with_default_capacity();
        let mut rx = bus.subscribe();
        bus.publish(ev(SourceClass::Core, "t", "k", None));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.cursor, 1);
        assert_eq!(got.kind, "k");
    }

    #[tokio::test]
    async fn slow_subscriber_lags() {
        // Verify the broadcast layer reports `Lagged` once a slow consumer
        // falls behind a publisher running ahead at full ring capacity. The
        // forwarder converts this into a "drop subscriber" decision.
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        for i in 0..32u64 {
            bus.publish(ev(SourceClass::Core, "t", &format!("k{i}"), None));
        }
        let mut lagged = false;
        for _ in 0..32 {
            match rx.try_recv() {
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    lagged = true;
                    break;
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                _ => {}
            }
        }
        assert!(lagged, "expected broadcast::TryRecvError::Lagged");
    }
}
