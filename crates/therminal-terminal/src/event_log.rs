//! Per-session JSONL event log for structured diagnostics.
//!
//! Each session writes to `$XDG_RUNTIME_DIR/therminal/sessions/<id>.events.jsonl`.
//! Events capture the semantic lifecycle of a session: spawn, status changes,
//! command start/finish, resize, PTY EOF, and bell.
//!
//! The log uses a simple truncate-on-overflow rotation strategy: when the
//! entry count exceeds `max_entries`, the file is truncated and writing
//! restarts from the beginning.

use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Default maximum entries before truncate-rotation.
pub const DEFAULT_MAX_ENTRIES: usize = 5000;

/// Semantic events in a session's lifecycle.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    Spawn {
        command: String,
        cwd: String,
    },
    StatusChange {
        old: String,
        new: String,
    },
    CommandStart {
        command: String,
    },
    CommandFinish {
        command: String,
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    PtyEof {
        reason: String,
    },
    Bell,
}

/// A single log line: ISO 8601 timestamp + flattened event fields.
#[derive(Debug, Serialize)]
struct LogEntry<'a> {
    ts: String,
    #[serde(flatten)]
    event: &'a SessionEvent,
}

/// A timestamped, in-memory copy of a [`SessionEvent`].
///
/// Returned by [`EventLog::snapshot`] for read-only consumers (MCP tool
/// `terminal.panes.query_events`). The `timestamp_secs` is recorded at the
/// moment [`EventLog::log`] was called and matches (truncated to seconds)
/// the `ts` field of the JSONL line.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub timestamp_secs: u64,
    pub event: SessionEvent,
}

/// Append-only JSONL writer with truncate-on-overflow rotation.
///
/// Also keeps a bounded in-memory ring buffer of recent events
/// (capped at `max_entries`, same cap as the file rotation) so callers
/// can fetch a structured snapshot without reading the JSONL file. The
/// in-memory buffer is the data source for the
/// `terminal.panes.query_events` MCP tool.
pub struct EventLog {
    writer: Option<BufWriter<File>>,
    count: usize,
    max_entries: usize,
    path: PathBuf,
    /// Bounded in-memory ring of recent events. Newest entries pushed at
    /// the back; oldest evicted from the front when `max_entries` is hit.
    buffer: VecDeque<StoredEvent>,
}

impl EventLog {
    /// Open (or create) a JSONL event log at `path`.
    ///
    /// The parent directory is created if it does not exist. The file is
    /// opened in append mode so existing entries are preserved across
    /// daemon restarts (until rotation truncates them).
    pub fn new(path: PathBuf, max_entries: usize) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // Estimate existing entry count from file size (avoid full parse).
        // Each JSONL line is roughly 100-300 bytes; we use a conservative
        // estimate to avoid premature rotation on restart.
        let existing = file
            .metadata()
            .map(|m| {
                let size = m.len();
                if size == 0 { 0 } else { (size / 150) as usize }
            })
            .unwrap_or(0);
        debug!(path = %path.display(), existing_estimate = existing, "Opened event log");
        Ok(Self {
            writer: Some(BufWriter::new(file)),
            count: existing,
            max_entries,
            path,
            buffer: VecDeque::with_capacity(max_entries.min(1024)),
        })
    }

    /// Create an in-memory-only event log with no file backing.
    ///
    /// Used by consumers (e.g. the daemon `Pane`) that only need the
    /// structured snapshot accessor and do not want JSONL file output.
    /// `log()` populates the bounded in-memory ring; the file path is
    /// recorded as empty and never opened.
    pub fn in_memory(max_entries: usize) -> Self {
        Self {
            writer: None,
            count: 0,
            max_entries,
            path: PathBuf::new(),
            buffer: VecDeque::with_capacity(max_entries.min(1024)),
        }
    }

    /// Create an event log in the standard session directory.
    ///
    /// Path: `$XDG_RUNTIME_DIR/therminal/sessions/<session_id>.events.jsonl`
    ///
    /// Falls back to `/tmp/therminal-<user>/sessions/` when `XDG_RUNTIME_DIR`
    /// is not set, using `$USER` for a per-user namespace (cross-platform).
    pub fn for_session(session_id: &str, max_entries: usize) -> std::io::Result<Self> {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "unknown".to_string());
            format!("/tmp/therminal-{user}")
        });
        let path = PathBuf::from(runtime_dir)
            .join("therminal")
            .join("sessions")
            .join(format!("{session_id}.events.jsonl"));
        Self::new(path, max_entries)
    }

    /// Write a single event as a JSON line, flush immediately.
    ///
    /// If the entry count exceeds `max_entries`, the file is truncated
    /// first (simple rotation).
    pub fn log(&mut self, event: &SessionEvent) {
        if self.count >= self.max_entries {
            self.rotate();
        }

        // Always push into the in-memory ring buffer, regardless of whether
        // a file writer is attached.
        let timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if self.buffer.len() >= self.max_entries {
            self.buffer.pop_front();
        }
        self.buffer.push_back(StoredEvent {
            timestamp_secs,
            event: event.clone(),
        });

        let Some(writer) = self.writer.as_mut() else {
            self.count += 1;
            return;
        };

        let entry = LogEntry {
            ts: now_iso8601(),
            event,
        };

        match serde_json::to_string(&entry) {
            Ok(json) => {
                if let Err(e) = writeln!(writer, "{json}") {
                    warn!(error = %e, "Failed to write event log entry");
                    return;
                }
                if let Err(e) = writer.flush() {
                    warn!(error = %e, "Failed to flush event log");
                    return;
                }
                self.count += 1;
            }
            Err(e) => {
                warn!(error = %e, "Failed to serialize event log entry");
            }
        }
    }

    /// Snapshot recent events from the in-memory ring buffer.
    ///
    /// Filters by `since_timestamp_secs` (inclusive — events at or after
    /// the given Unix timestamp), then keeps the **newest** `limit`
    /// entries, returned **oldest-first** for natural transcript order.
    ///
    /// Returns at most `min(limit, max_entries)` events. The buffer is
    /// already capped at `max_entries` (default 5000).
    pub fn snapshot(&self, since_timestamp_secs: Option<u64>, limit: usize) -> Vec<StoredEvent> {
        let cap = limit.min(self.max_entries);
        if cap == 0 {
            return Vec::new();
        }
        let filtered: Vec<&StoredEvent> = self
            .buffer
            .iter()
            .filter(|e| match since_timestamp_secs {
                Some(since) => e.timestamp_secs >= since,
                None => true,
            })
            .collect();
        let total = filtered.len();
        let start = total.saturating_sub(cap);
        filtered[start..].iter().map(|e| (*e).clone()).collect()
    }

    /// Return the path to the event log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Truncate the file and reset the entry count (simple rotation).
    fn rotate(&mut self) {
        debug!(path = %self.path.display(), entries = self.count, "Rotating event log (truncate)");
        if self.writer.is_none() {
            // In-memory only — the ring buffer takes care of bounding.
            self.count = 0;
            return;
        }
        // Re-open the file in truncate mode.
        match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.writer = Some(BufWriter::new(file));
                self.count = 0;
            }
            Err(e) => {
                warn!(error = %e, "Failed to rotate event log, continuing with existing file");
            }
        }
    }

    /// Remove the event log file from disk.
    pub fn remove(path: &Path) {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(path) {
                warn!(path = %path.display(), error = %e, "Failed to remove event log file");
            } else {
                debug!(path = %path.display(), "Removed event log file");
            }
        }
    }
}

/// ISO 8601 timestamp in UTC (no external crate dependency).
fn now_iso8601() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Gregorian calendar conversion from days since epoch.
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's `civil_from_days`.
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u64, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn write_and_read_events() {
        let dir = std::env::temp_dir().join("therminal-event-log-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.events.jsonl");

        let mut log = EventLog::new(path.clone(), 100).unwrap();
        log.log(&SessionEvent::Spawn {
            command: "bash".into(),
            cwd: "/home/user".into(),
        });
        log.log(&SessionEvent::StatusChange {
            old: "idle".into(),
            new: "processing".into(),
        });
        log.log(&SessionEvent::Bell);

        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        let lines: Vec<&str> = contents.trim().lines().collect();
        assert_eq!(lines.len(), 3);

        // Verify each line is valid JSON with a `ts` and `event` field.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("ts").is_some());
            assert!(v.get("event").is_some());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_truncates() {
        let dir = std::env::temp_dir().join("therminal-event-log-rotate-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rotate.events.jsonl");

        let mut log = EventLog::new(path.clone(), 3).unwrap();
        for _ in 0..5 {
            log.log(&SessionEvent::Bell);
        }

        let mut contents = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        let lines: Vec<&str> = contents.trim().lines().collect();
        // After 3 entries, rotation truncates. Then 2 more are written.
        assert_eq!(lines.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_memory_snapshot_respects_limit() {
        let mut log = EventLog::in_memory(100);
        for _ in 0..5 {
            log.log(&SessionEvent::Bell);
        }
        let snap = log.snapshot(None, 3);
        assert_eq!(snap.len(), 3);
        // Limit > buffer returns all.
        let snap = log.snapshot(None, 100);
        assert_eq!(snap.len(), 5);
    }

    #[test]
    fn in_memory_snapshot_filters_since_timestamp() {
        let mut log = EventLog::in_memory(100);
        log.log(&SessionEvent::Bell);
        let snap = log.snapshot(Some(0), 10);
        assert_eq!(snap.len(), 1);
        // Filter at u64::MAX yields nothing.
        let snap = log.snapshot(Some(u64::MAX), 10);
        assert_eq!(snap.len(), 0);
    }

    #[test]
    fn in_memory_buffer_caps_at_max_entries() {
        let mut log = EventLog::in_memory(3);
        for _ in 0..7 {
            log.log(&SessionEvent::Bell);
        }
        let snap = log.snapshot(None, 100);
        assert_eq!(snap.len(), 3, "ring buffer must cap at max_entries");
    }

    #[test]
    fn empty_log_snapshot_is_empty() {
        let log = EventLog::in_memory(100);
        assert!(log.snapshot(None, 100).is_empty());
    }

    #[test]
    fn days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_date() {
        // 2024-01-01 is day 19723
        assert_eq!(days_to_ymd(19723), (2024, 1, 1));
    }
}
