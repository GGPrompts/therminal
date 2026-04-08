//! `claude-events` — CLI viewer for the `therminal://claude/events` MCP resource.
//!
//! Stopgap visibility tool for Claude Code session observability while the GPU
//! timeline overlay widget waits on Phase 6 overlay infrastructure (tn-x85k).
//!
//! Connects to the running therminal daemon's MCP socket, performs a minimal
//! JSON-RPC handshake, subscribes to `therminal://claude/events`, drains the
//! initial buffer, and prints styled lines for every `TaggedAgentEvent` that
//! arrives via `notifications/resources/updated`.
//!
//! Run via: `cargo run -p therminal-daemon --bin claude-events`. Flags and
//! output format are documented in the repo README under "Dev tools".

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use therminal_harness_claude::agent_events::AgentEvent;
use therminal_harness_claude::jsonl_tailer::{EventSource, TaggedAgentEvent};

/// Local mirror of [`TaggedAgentEvent`] that derives `Deserialize`. The
/// upstream type is `Serialize`-only because nothing else in the daemon
/// needs to round-trip it. We convert to the canonical type after parsing.
#[derive(Debug, Deserialize)]
struct TaggedAgentEventDe {
    event: AgentEventDe,
    source: EventSourceDe,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EventSourceDe {
    TopLevel {
        session_id: String,
    },
    Subagent {
        parent_session_id: String,
        agent_id: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AgentEventDe {
    UserMessage {
        content: String,
    },
    AssistantMessage {
        content: String,
    },
    ToolUse {
        tool: String,
        input: Value,
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    ToolResult {
        tool: String,
        output: String,
        is_error: bool,
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    Progress {
        tool: String,
        status: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    Thinking {
        content: String,
    },
}

impl From<TaggedAgentEventDe> for TaggedAgentEvent {
    fn from(d: TaggedAgentEventDe) -> Self {
        let event = match d.event {
            AgentEventDe::UserMessage { content } => AgentEvent::UserMessage { content },
            AgentEventDe::AssistantMessage { content } => AgentEvent::AssistantMessage { content },
            AgentEventDe::ToolUse {
                tool,
                input,
                tool_use_id,
            } => AgentEvent::ToolUse {
                tool,
                input,
                tool_use_id,
            },
            AgentEventDe::ToolResult {
                tool,
                output,
                is_error,
                tool_use_id,
            } => AgentEvent::ToolResult {
                tool,
                output,
                is_error,
                tool_use_id,
            },
            AgentEventDe::Progress {
                tool,
                status,
                message,
                tool_use_id,
            } => AgentEvent::Progress {
                tool,
                status,
                message,
                tool_use_id,
            },
            AgentEventDe::Thinking { content } => AgentEvent::Thinking { content },
        };
        let source = match d.source {
            EventSourceDe::TopLevel { session_id } => EventSource::TopLevel { session_id },
            EventSourceDe::Subagent {
                parent_session_id,
                agent_id,
            } => EventSource::Subagent {
                parent_session_id,
                agent_id,
            },
        };
        TaggedAgentEvent { event, source }
    }
}
use therminal_daemon_client::ipc_transport::{IpcClientStream, connect_client};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

const CLAUDE_EVENTS_URI: &str = "therminal://claude/events";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterMode {
    Top,
    Sub,
    All,
}

#[derive(Debug, Clone)]
struct Cli {
    filter: FilterMode,
    session: Option<String>,
    verbose: bool,
    no_color: bool,
    json: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            filter: FilterMode::All,
            session: None,
            verbose: false,
            no_color: false,
            json: false,
        }
    }
}

fn print_help() {
    println!(
        "claude-events — live viewer for therminal://claude/events MCP resource\n\
\n\
USAGE:\n    \
    claude-events [FLAGS]\n\
\n\
FLAGS:\n    \
    --filter <top|sub|all>   Show only top-level, subagent, or all events (default: all)\n    \
    --session <sid>          Filter to one session id (matches top-level or its subagents)\n    \
    --verbose                Include UserMessage and AssistantMessage events (suppressed by default)\n    \
    --no-color               Disable ANSI color codes\n    \
    --json                   Print raw TaggedAgentEvent JSON, one per line (for jq)\n    \
    --help                   Show this help message\n\
\n\
This is a stopgap dev tool while the GPU timeline overlay widget (tn-x85k) is built."
    );
}

fn parse_args() -> Result<Option<Cli>> {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--verbose" => cli.verbose = true,
            "--no-color" => cli.no_color = true,
            "--json" => cli.json = true,
            "--filter" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--filter requires a value (top|sub|all)"))?;
                cli.filter = match v.as_str() {
                    "top" => FilterMode::Top,
                    "sub" => FilterMode::Sub,
                    "all" => FilterMode::All,
                    other => return Err(anyhow!("invalid --filter value: {other}")),
                };
            }
            "--session" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--session requires a value"))?;
                cli.session = Some(v);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    Ok(Some(cli))
}

/// Returns true if the event should be shown given the CLI filter/session selectors.
fn event_passes(cli: &Cli, ev: &TaggedAgentEvent) -> bool {
    let is_top = matches!(ev.source, EventSource::TopLevel { .. });
    match cli.filter {
        FilterMode::Top if !is_top => return false,
        FilterMode::Sub if is_top => return false,
        _ => {}
    }
    if let Some(sid) = &cli.session {
        let matched = match &ev.source {
            EventSource::TopLevel { session_id } => session_id == sid,
            EventSource::Subagent {
                parent_session_id, ..
            } => parent_session_id == sid,
        };
        if !matched {
            return false;
        }
    }
    if !cli.verbose
        && matches!(
            ev.event,
            AgentEvent::UserMessage { .. } | AgentEvent::AssistantMessage { .. }
        )
    {
        return false;
    }
    true
}

// ---- ANSI color helpers ------------------------------------------------------

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";

fn paint(s: &str, color: &str, no_color: bool) -> String {
    if no_color {
        s.to_string()
    } else {
        format!("{color}{s}{RESET}")
    }
}

// ---- Formatting --------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    let one_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one_line.chars().count() <= max {
        one_line
    } else {
        let mut out: String = one_line.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Render a brief summary of a tool's input JSON. Tries known shapes first
/// then falls back to the trimmed Display of the raw value.
fn summarize_tool_input(tool: &str, input: &Value) -> String {
    let try_field = |field: &str| input.get(field).and_then(|v| v.as_str()).map(String::from);
    let summary = match tool {
        "Bash" => try_field("command"),
        "Read" | "Edit" | "Write" | "MultiEdit" => try_field("file_path"),
        "Grep" | "Glob" => try_field("pattern"),
        "WebFetch" | "WebSearch" => try_field("url").or_else(|| try_field("query")),
        _ => None,
    };
    match summary {
        Some(s) => truncate(&s, 80),
        None => truncate(&input.to_string(), 60),
    }
}

fn local_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // local time without pulling chrono — use the `time` crate already in workspace.
    // We use UTC here — the workspace `time` crate isn't built with the
    // `local-offset` feature, so true local time would require an extra dep.
    // For a dev tool that's an acceptable tradeoff.
    match time::OffsetDateTime::from_unix_timestamp(secs as i64) {
        Ok(t) => format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second()),
        Err(_) => "00:00:00".to_string(),
    }
}

fn format_event_line(cli: &Cli, ev: &TaggedAgentEvent, now_hms: &str) -> String {
    let no_color = cli.no_color;
    let (tag_label, indent, id_short) = match &ev.source {
        EventSource::TopLevel { session_id } => ("top", "", short8(session_id)),
        EventSource::Subagent { agent_id, .. } => ("sub", "  ", short8(agent_id)),
    };
    let tag_raw = format!("[{tag_label} {id_short}]");
    let tag = if matches!(ev.source, EventSource::Subagent { .. }) {
        paint(&tag_raw, DIM, no_color)
    } else {
        tag_raw
    };

    let (kind, payload) = match &ev.event {
        AgentEvent::ToolUse { tool, input, .. } => (
            paint(&format!("{:<12}", tool), CYAN, no_color),
            summarize_tool_input(tool, input),
        ),
        AgentEvent::ToolResult {
            output, is_error, ..
        } => {
            let sym = if *is_error { "✗" } else { "✓" };
            let color = if *is_error { RED } else { GREEN };
            (
                paint(&format!("{:<12}", sym), color, no_color),
                truncate(output, 80),
            )
        }
        AgentEvent::Progress {
            tool,
            status,
            message,
            ..
        } => {
            let label = format!("{tool}:{status}");
            (
                paint(&format!("{:<12}", label), DIM, no_color),
                truncate(message.as_deref().unwrap_or(""), 80),
            )
        }
        AgentEvent::Thinking { content } => (
            paint(&format!("{:<12}", "💭"), YELLOW, no_color),
            paint(&truncate(content, 40), YELLOW, no_color),
        ),
        AgentEvent::UserMessage { content } => (
            paint(&format!("{:<12}", "user"), BLUE, no_color),
            truncate(content, 80),
        ),
        AgentEvent::AssistantMessage { content } => (
            paint(&format!("{:<12}", "assistant"), MAGENTA, no_color),
            truncate(content, 80),
        ),
    };

    format!("{now_hms} {tag}{indent}  {kind} {payload}")
}

fn short8(s: &str) -> String {
    s.chars().take(8).collect()
}

// ---- IPC: minimal JSON-RPC over the daemon MCP socket ----------------------

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    id: Option<Value>,
    result: Option<Value>,
    error: Option<Value>,
}

/// Connect to the daemon's MCP socket, returning a cross-platform stream.
///
/// Uses `therminal_daemon_client::ipc_transport::connect_client` so the same
/// code path works on Unix domain sockets and Windows named pipes. `claude-events`
/// speaks newline-delimited JSON-RPC directly over the raw stream — the
/// MessagePack `IpcMessage` envelope that `DaemonClient` wraps around the
/// daemon *control* socket does not apply here.
async fn open_stream() -> Result<IpcClientStream> {
    let config = therminal_core::config::TherminalConfig::load();
    let socket_path = config.mcp.resolved_socket_path();
    connect_client(&socket_path).await.with_context(|| {
        format!(
            "failed to connect to daemon MCP socket at {}. Is the therminal daemon running?",
            socket_path.display()
        )
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = match parse_args()? {
        Some(c) => c,
        None => return Ok(()),
    };

    let stream = open_stream().await?;
    // tokio::io::split is cross-platform — UnixStream::into_split yields
    // Unix-only owned halves, while named pipes need io::split. Both sides
    // implement AsyncRead / AsyncWrite so BufReader::lines and write_all
    // work identically.
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();

    // ---- handshake ----
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "claude-events", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    send_json(&mut write_half, &init).await?;
    let _init_resp = read_response(&mut reader, 1).await?;

    // notifications/initialized
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    send_json(&mut write_half, &initialized).await?;

    // resources/subscribe
    let sub = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "resources/subscribe",
        "params": { "uri": CLAUDE_EVENTS_URI }
    });
    send_json(&mut write_half, &sub).await?;
    let _sub_resp = read_response(&mut reader, 2).await?;

    // initial drain
    drain_and_print(&cli, &mut write_half, &mut reader, 3).await?;

    // ---- loop on notifications ----
    let mut next_id: u64 = 4;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nclaude-events: shutting down");
                return Ok(());
            }
            line = reader.next_line() => {
                let line = match line {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        eprintln!("claude-events: daemon closed connection");
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                };
                if line.trim().is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("claude-events: failed to parse server message: {e}");
                        continue;
                    }
                };
                let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                if method == "notifications/resources/updated" {
                    let uri = msg
                        .get("params")
                        .and_then(|p| p.get("uri"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");
                    if uri == CLAUDE_EVENTS_URI {
                        drain_and_print(&cli, &mut write_half, &mut reader, next_id).await?;
                        next_id += 1;
                    }
                }
            }
        }
    }
}

async fn send_json(write_half: &mut WriteHalf<IpcClientStream>, value: &Value) -> Result<()> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    write_half.write_all(&buf).await?;
    write_half.flush().await?;
    Ok(())
}

async fn read_response(
    reader: &mut tokio::io::Lines<BufReader<ReadHalf<IpcClientStream>>>,
    expect_id: u64,
) -> Result<JsonRpcResponse> {
    loop {
        let line = reader
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed connection during handshake"))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON-RPC line: {line}"))?;
        // Skip notifications while waiting for a response.
        if v.get("id").is_none() {
            continue;
        }
        if v.get("id").and_then(|i| i.as_u64()) != Some(expect_id) {
            continue;
        }
        let resp: JsonRpcResponse = serde_json::from_value(v)?;
        if let Some(err) = &resp.error {
            return Err(anyhow!("daemon returned JSON-RPC error: {err}"));
        }
        return Ok(resp);
    }
}

async fn drain_and_print(
    cli: &Cli,
    write_half: &mut WriteHalf<IpcClientStream>,
    reader: &mut tokio::io::Lines<BufReader<ReadHalf<IpcClientStream>>>,
    id: u64,
) -> Result<()> {
    let read_req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "resources/read",
        "params": { "uri": CLAUDE_EVENTS_URI }
    });
    send_json(write_half, &read_req).await?;
    let resp = read_response(reader, id).await?;
    let result = match resp.result {
        Some(r) => r,
        None => return Ok(()),
    };
    let contents = match result.get("contents").and_then(|c| c.as_array()) {
        Some(a) => a,
        None => return Ok(()),
    };
    for entry in contents {
        let text = match entry.get("text").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => continue,
        };
        let events: Vec<TaggedAgentEvent> =
            match serde_json::from_str::<Vec<TaggedAgentEventDe>>(text) {
                Ok(e) => e.into_iter().map(Into::into).collect(),
                Err(e) => {
                    eprintln!("claude-events: failed to parse events payload: {e}");
                    continue;
                }
            };
        let now = local_hms();
        for ev in events {
            if !event_passes(cli, &ev) {
                continue;
            }
            if cli.json {
                if let Ok(s) = serde_json::to_string(&ev) {
                    println!("{s}");
                }
            } else {
                println!("{}", format_event_line(cli, &ev, &now));
            }
        }
    }
    Ok(())
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // skip until 'm'
                for c2 in chars.by_ref() {
                    if c2 == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn cli_default() -> Cli {
        Cli::default()
    }

    #[test]
    fn formats_tooluse_top_level() {
        let ev = TaggedAgentEvent {
            event: AgentEvent::ToolUse {
                tool: "Bash".into(),
                input: json!({ "command": "ls -la" }),
                tool_use_id: None,
            },
            source: EventSource::TopLevel {
                session_id: "abc12345xxxxxxxx".into(),
            },
        };
        let cli = cli_default();
        let line = format_event_line(&cli, &ev, "12:34:56");
        let plain = strip_ansi(&line);
        assert!(plain.contains("12:34:56"));
        assert!(plain.contains("[top abc12345]"));
        assert!(plain.contains("Bash"));
        assert!(plain.contains("ls -la"));
    }

    #[test]
    fn formats_subagent_with_indent_and_tag() {
        let ev = TaggedAgentEvent {
            event: AgentEvent::ToolUse {
                tool: "Grep".into(),
                input: json!({ "pattern": "jsonl_tailer" }),
                tool_use_id: None,
            },
            source: EventSource::Subagent {
                parent_session_id: "abc12345aaaa".into(),
                agent_id: "7ee3d21abbbb".into(),
            },
        };
        let cli = Cli {
            no_color: true,
            ..Cli::default()
        };
        let line = format_event_line(&cli, &ev, "01:02:03");
        assert!(line.contains("[sub 7ee3d21a]"));
        // Subagent prefix gets two extra spaces of indent before the kind column.
        assert!(line.contains("[sub 7ee3d21a]  "));
        assert!(line.contains("Grep"));
        assert!(line.contains("jsonl_tailer"));
    }

    #[test]
    fn json_mode_roundtrips() {
        let ev = TaggedAgentEvent {
            event: AgentEvent::Thinking {
                content: "hmm".into(),
            },
            source: EventSource::TopLevel {
                session_id: "sessabcd".into(),
            },
        };
        let s = serde_json::to_string(&ev).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["source"]["kind"], "top_level");
        assert_eq!(v["event"]["type"], "thinking");
    }

    #[test]
    fn filter_top_excludes_subagents() {
        let cli = Cli {
            filter: FilterMode::Top,
            ..Cli::default()
        };
        let sub = TaggedAgentEvent {
            event: AgentEvent::ToolUse {
                tool: "Bash".into(),
                input: json!({}),
                tool_use_id: None,
            },
            source: EventSource::Subagent {
                parent_session_id: "p".into(),
                agent_id: "a".into(),
            },
        };
        let top = TaggedAgentEvent {
            event: AgentEvent::ToolUse {
                tool: "Bash".into(),
                input: json!({}),
                tool_use_id: None,
            },
            source: EventSource::TopLevel {
                session_id: "p".into(),
            },
        };
        assert!(!event_passes(&cli, &sub));
        assert!(event_passes(&cli, &top));
    }

    #[test]
    fn session_filter_matches_parent_or_top() {
        let cli = Cli {
            session: Some("target".into()),
            ..Cli::default()
        };
        let sub_match = TaggedAgentEvent {
            event: AgentEvent::ToolUse {
                tool: "X".into(),
                input: json!({}),
                tool_use_id: None,
            },
            source: EventSource::Subagent {
                parent_session_id: "target".into(),
                agent_id: "x".into(),
            },
        };
        let sub_other = TaggedAgentEvent {
            event: AgentEvent::ToolUse {
                tool: "X".into(),
                input: json!({}),
                tool_use_id: None,
            },
            source: EventSource::Subagent {
                parent_session_id: "other".into(),
                agent_id: "x".into(),
            },
        };
        let top_match = TaggedAgentEvent {
            event: AgentEvent::ToolUse {
                tool: "X".into(),
                input: json!({}),
                tool_use_id: None,
            },
            source: EventSource::TopLevel {
                session_id: "target".into(),
            },
        };
        assert!(event_passes(&cli, &sub_match));
        assert!(!event_passes(&cli, &sub_other));
        assert!(event_passes(&cli, &top_match));
    }

    #[test]
    fn verbose_gates_user_assistant_messages() {
        let ev = TaggedAgentEvent {
            event: AgentEvent::AssistantMessage {
                content: "hello".into(),
            },
            source: EventSource::TopLevel {
                session_id: "s".into(),
            },
        };
        assert!(!event_passes(&Cli::default(), &ev));
        let cli = Cli {
            verbose: true,
            ..Cli::default()
        };
        assert!(event_passes(&cli, &ev));
    }
}
