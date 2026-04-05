//! Control mode — a text-based machine-readable protocol for driving Therminal
//! programmatically, similar to tmux's `-CC` mode.
//!
//! External tools (claude-squad, agent-deck, etc.) connect via the daemon's IPC
//! socket and send text commands. Responses are wrapped in `%begin`/`%end` blocks.
//! Async notifications are prefixed with `%`.
//!
//! ## Protocol Format
//!
//! ```text
//! new-session -n mywork
//! %begin 1
//! {"session_id":"sess-abc123"}
//! %end 1
//!
//! %session-changed sess-abc123
//! ```
//!
//! ## Commands
//!
//! - `new-session [-n NAME]` — create a new session
//! - `list-sessions` — list all session IDs
//! - `send-keys PANE_ID KEYS...` — send input to a pane
//! - `split-pane [-h|-v] PANE_ID` — split a pane (default: vertical)
//! - `select-pane PANE_ID` — focus a pane
//! - `capture-pane PANE_ID [-p]` — get pane content
//! - `kill-pane PANE_ID` — close a pane
//! - `list-panes SESSION_ID` — list panes in a session
//! - `ping` — health check
//! - `exit` — close the control connection

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use therminal_protocol::daemon::DaemonEvent;

use crate::lifecycle::Lifecycle;
use crate::session::SessionManager;

/// A single parsed control-mode command.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlCommand {
    /// `new-session [-n NAME]`
    NewSession { name: Option<String> },
    /// `list-sessions`
    ListSessions,
    /// `send-keys PANE_ID KEYS...`
    SendKeys { pane_id: String, keys: String },
    /// `split-pane [-h|-v] PANE_ID`
    SplitPane { pane_id: String, horizontal: bool },
    /// `select-pane PANE_ID`
    SelectPane { pane_id: String },
    /// `capture-pane PANE_ID [-p]`
    CapturePane { pane_id: String, print_mode: bool },
    /// `kill-pane PANE_ID`
    KillPane { pane_id: String },
    /// `list-panes SESSION_ID`
    ListPanes { session_id: String },
    /// `ping`
    Ping,
    /// `exit`
    Exit,
}

/// Parse error for control mode input.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Parse a single line of control-mode input into a `ControlCommand`.
pub fn parse_command(line: &str) -> Result<ControlCommand, ParseError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(ParseError {
            message: "empty command".to_string(),
        });
    }

    let parts: Vec<&str> = line.split_whitespace().collect();
    let cmd = parts[0];

    match cmd {
        "new-session" => {
            let mut name = None;
            let mut i = 1;
            while i < parts.len() {
                if parts[i] == "-n" && i + 1 < parts.len() {
                    name = Some(parts[i + 1].to_string());
                    i += 2;
                } else {
                    return Err(ParseError {
                        message: format!("unknown flag: {}", parts[i]),
                    });
                }
            }
            Ok(ControlCommand::NewSession { name })
        }
        "list-sessions" => Ok(ControlCommand::ListSessions),
        "send-keys" => {
            if parts.len() < 3 {
                return Err(ParseError {
                    message: "usage: send-keys PANE_ID KEYS...".to_string(),
                });
            }
            let pane_id = parts[1].to_string();
            // Everything after pane_id is the keys (rejoin with spaces)
            let keys = parts[2..].join(" ");
            Ok(ControlCommand::SendKeys { pane_id, keys })
        }
        "split-pane" => {
            let mut horizontal = false;
            let mut pane_id = None;
            let mut i = 1;
            while i < parts.len() {
                match parts[i] {
                    "-h" => {
                        horizontal = true;
                        i += 1;
                    }
                    "-v" => {
                        horizontal = false;
                        i += 1;
                    }
                    _ => {
                        pane_id = Some(parts[i].to_string());
                        i += 1;
                    }
                }
            }
            let pane_id = pane_id.ok_or_else(|| ParseError {
                message: "usage: split-pane [-h|-v] PANE_ID".to_string(),
            })?;
            Ok(ControlCommand::SplitPane {
                pane_id,
                horizontal,
            })
        }
        "select-pane" => {
            if parts.len() < 2 {
                return Err(ParseError {
                    message: "usage: select-pane PANE_ID".to_string(),
                });
            }
            Ok(ControlCommand::SelectPane {
                pane_id: parts[1].to_string(),
            })
        }
        "capture-pane" => {
            if parts.len() < 2 {
                return Err(ParseError {
                    message: "usage: capture-pane PANE_ID [-p]".to_string(),
                });
            }
            let pane_id = parts[1].to_string();
            let print_mode = parts[2..].contains(&"-p");
            Ok(ControlCommand::CapturePane {
                pane_id,
                print_mode,
            })
        }
        "kill-pane" => {
            if parts.len() < 2 {
                return Err(ParseError {
                    message: "usage: kill-pane PANE_ID".to_string(),
                });
            }
            Ok(ControlCommand::KillPane {
                pane_id: parts[1].to_string(),
            })
        }
        "list-panes" => {
            if parts.len() < 2 {
                return Err(ParseError {
                    message: "usage: list-panes SESSION_ID".to_string(),
                });
            }
            Ok(ControlCommand::ListPanes {
                session_id: parts[1].to_string(),
            })
        }
        "ping" => Ok(ControlCommand::Ping),
        "exit" => Ok(ControlCommand::Exit),
        _ => Err(ParseError {
            message: format!("unknown command: {cmd}"),
        }),
    }
}

/// Format a `%begin`/`%end` response block.
pub fn format_response(request_id: u64, body: &str) -> String {
    format!("%begin {request_id}\n{body}\n%end {request_id}\n")
}

/// Format an error response.
pub fn format_error(request_id: u64, message: &str) -> String {
    format!("%begin {request_id}\n%error {message}\n%end {request_id}\n")
}

/// Format an async notification.
pub fn format_notification(event: &DaemonEvent) -> String {
    match event {
        DaemonEvent::SessionCreated { session_id } => {
            format!("%session-changed {session_id}\n")
        }
        DaemonEvent::SessionDestroyed { session_id } => {
            format!("%session-closed {session_id}\n")
        }
        DaemonEvent::StateChanged { old, new } => {
            format!("%state-changed {old} {new}\n")
        }
        DaemonEvent::PaneOutput {
            pane_id,
            session_id: _,
            data: _,
        } => {
            format!("%pane-output {pane_id}\n")
        }
    }
}

/// Handle a control-mode connection.
///
/// This reads text commands line-by-line from the stream, dispatches them
/// to the session manager, and writes back `%begin`/`%end` response blocks.
/// Async events are forwarded as `%`-prefixed notifications.
pub async fn handle_control_connection(
    stream: UnixStream,
    lifecycle: Arc<Lifecycle>,
    event_tx: broadcast::Sender<DaemonEvent>,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    build_hash: String,
    version: String,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut event_rx = event_tx.subscribe();
    let mut request_id: u64 = 0;

    // Send initial greeting
    let greeting = format!(
        "%begin 0\n{{\"mode\":\"control\",\"version\":\"{version}\",\"build_hash\":\"{build_hash}\"}}\n%end 0\n"
    );
    if writer.write_all(greeting.as_bytes()).await.is_err() {
        return;
    }
    let _ = writer.flush().await;

    loop {
        tokio::select! {
            line_result = lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        request_id += 1;
                        let rid = request_id;

                        let cmd = match parse_command(&line) {
                            Ok(cmd) => cmd,
                            Err(e) => {
                                let resp = format_error(rid, &e.message);
                                if writer.write_all(resp.as_bytes()).await.is_err() {
                                    break;
                                }
                                let _ = writer.flush().await;
                                continue;
                            }
                        };

                        if cmd == ControlCommand::Exit {
                            let resp = format_response(rid, "goodbye");
                            let _ = writer.write_all(resp.as_bytes()).await;
                            let _ = writer.flush().await;
                            break;
                        }

                        let response = dispatch_control_command(
                            &cmd, &lifecycle, &session_mgr, &build_hash, &version,
                        ).await;

                        let resp = match response {
                            Ok(body) => format_response(rid, &body),
                            Err(msg) => format_error(rid, &msg),
                        };

                        if writer.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = writer.flush().await;
                    }
                    Ok(None) => break, // EOF
                    Err(e) => {
                        debug!(error = %e, "control connection read error");
                        break;
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(evt) => {
                        let notification = format_notification(&evt);
                        if writer.write_all(notification.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = writer.flush().await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "control mode event subscriber lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    debug!("control connection closed");
}

/// Dispatch a parsed control command and return the response body or error.
async fn dispatch_control_command(
    cmd: &ControlCommand,
    lifecycle: &Arc<Lifecycle>,
    session_mgr: &Arc<tokio::sync::Mutex<SessionManager>>,
    build_hash: &str,
    version: &str,
) -> Result<String, String> {
    match cmd {
        ControlCommand::Ping => {
            let mgr = session_mgr.lock().await;
            Ok(format!(
                "{{\"status\":\"ok\",\"version\":\"{}\",\"build_hash\":\"{}\",\"uptime_secs\":{},\"sessions\":{}}}",
                version, build_hash, lifecycle.uptime_secs(), mgr.session_count()
            ))
        }
        ControlCommand::NewSession { name } => {
            let mut mgr = session_mgr.lock().await;
            match mgr.create_session(name.clone()) {
                Ok(session_id) => {
                    lifecycle.set_session_count(mgr.session_count());
                    Ok(format!("{{\"session_id\":\"{session_id}\"}}"))
                }
                Err(e) => Err(format!("failed to create session: {e}")),
            }
        }
        ControlCommand::ListSessions => {
            let mgr = session_mgr.lock().await;
            let ids = mgr.list_sessions();
            let json_ids: Vec<String> = ids.iter().map(|id| format!("\"{id}\"")).collect();
            Ok(format!("[{}]", json_ids.join(",")))
        }
        ControlCommand::SendKeys { pane_id, keys } => {
            let mut mgr = session_mgr.lock().await;
            mgr.send_keys_to_pane(pane_id, keys.as_bytes())?;
            Ok(format!("{{\"pane_id\":\"{pane_id}\",\"sent\":true}}"))
        }
        ControlCommand::SplitPane {
            pane_id,
            horizontal,
        } => {
            let mut mgr = session_mgr.lock().await;
            let new_id = mgr.split_pane(pane_id, *horizontal)?;
            Ok(format!("{{\"new_pane_id\":\"{new_id}\"}}"))
        }
        ControlCommand::SelectPane { pane_id } => {
            let mgr = session_mgr.lock().await;
            mgr.select_pane(pane_id)?;
            Ok(format!("{{\"pane_id\":\"{pane_id}\",\"selected\":true}}"))
        }
        ControlCommand::CapturePane {
            pane_id,
            print_mode,
        } => {
            let mgr = session_mgr.lock().await;
            let snap = mgr.capture_pane(pane_id)?;
            let lines: Vec<String> = snap
                .grid
                .iter()
                .map(|row| {
                    let s: String = row.iter().map(|(ch, _)| ch).collect();
                    s.trim_end().to_string()
                })
                .collect();

            if *print_mode {
                // Plain text output, one line per row
                Ok(lines.join("\n"))
            } else {
                // JSON output
                let json_lines: Vec<String> = lines
                    .iter()
                    .map(|l| format!("\"{}\"", l.replace('\\', "\\\\").replace('"', "\\\"")))
                    .collect();
                Ok(format!(
                    "{{\"pane_id\":\"{}\",\"cols\":{},\"rows\":{},\"cursor_col\":{},\"cursor_line\":{},\"lines\":[{}]}}",
                    snap.pane_id, snap.cols, snap.rows, snap.cursor_col, snap.cursor_line,
                    json_lines.join(",")
                ))
            }
        }
        ControlCommand::KillPane { pane_id } => {
            let mut mgr = session_mgr.lock().await;
            mgr.kill_pane(pane_id)?;
            lifecycle.set_session_count(mgr.session_count());
            Ok(format!("{{\"pane_id\":\"{pane_id}\",\"killed\":true}}"))
        }
        ControlCommand::ListPanes { session_id } => {
            let mgr = session_mgr.lock().await;
            match mgr.attach(session_id) {
                Some(snapshot) => {
                    let pane_entries: Vec<String> = snapshot
                        .panes
                        .iter()
                        .map(|p| {
                            format!(
                                "{{\"pane_id\":\"{}\",\"cols\":{},\"rows\":{}}}",
                                p.pane_id, p.cols, p.rows
                            )
                        })
                        .collect();
                    Ok(format!("[{}]", pane_entries.join(",")))
                }
                None => Err(format!("session not found: {session_id}")),
            }
        }
        ControlCommand::Exit => {
            // Handled in the main loop before dispatching
            unreachable!()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ping() {
        assert_eq!(parse_command("ping").unwrap(), ControlCommand::Ping);
    }

    #[test]
    fn parse_exit() {
        assert_eq!(parse_command("exit").unwrap(), ControlCommand::Exit);
    }

    #[test]
    fn parse_new_session_no_name() {
        assert_eq!(
            parse_command("new-session").unwrap(),
            ControlCommand::NewSession { name: None }
        );
    }

    #[test]
    fn parse_new_session_with_name() {
        assert_eq!(
            parse_command("new-session -n mywork").unwrap(),
            ControlCommand::NewSession {
                name: Some("mywork".to_string())
            }
        );
    }

    #[test]
    fn parse_list_sessions() {
        assert_eq!(
            parse_command("list-sessions").unwrap(),
            ControlCommand::ListSessions
        );
    }

    #[test]
    fn parse_send_keys() {
        assert_eq!(
            parse_command("send-keys pane-123 ls -la").unwrap(),
            ControlCommand::SendKeys {
                pane_id: "pane-123".to_string(),
                keys: "ls -la".to_string(),
            }
        );
    }

    #[test]
    fn parse_send_keys_missing_args() {
        assert!(parse_command("send-keys pane-123").is_err());
    }

    #[test]
    fn parse_split_pane_vertical() {
        assert_eq!(
            parse_command("split-pane pane-123").unwrap(),
            ControlCommand::SplitPane {
                pane_id: "pane-123".to_string(),
                horizontal: false,
            }
        );
    }

    #[test]
    fn parse_split_pane_horizontal() {
        assert_eq!(
            parse_command("split-pane -h pane-123").unwrap(),
            ControlCommand::SplitPane {
                pane_id: "pane-123".to_string(),
                horizontal: true,
            }
        );
    }

    #[test]
    fn parse_select_pane() {
        assert_eq!(
            parse_command("select-pane pane-abc").unwrap(),
            ControlCommand::SelectPane {
                pane_id: "pane-abc".to_string(),
            }
        );
    }

    #[test]
    fn parse_capture_pane_plain() {
        assert_eq!(
            parse_command("capture-pane pane-abc").unwrap(),
            ControlCommand::CapturePane {
                pane_id: "pane-abc".to_string(),
                print_mode: false,
            }
        );
    }

    #[test]
    fn parse_capture_pane_print_mode() {
        assert_eq!(
            parse_command("capture-pane pane-abc -p").unwrap(),
            ControlCommand::CapturePane {
                pane_id: "pane-abc".to_string(),
                print_mode: true,
            }
        );
    }

    #[test]
    fn parse_kill_pane() {
        assert_eq!(
            parse_command("kill-pane pane-xyz").unwrap(),
            ControlCommand::KillPane {
                pane_id: "pane-xyz".to_string(),
            }
        );
    }

    #[test]
    fn parse_list_panes() {
        assert_eq!(
            parse_command("list-panes sess-123").unwrap(),
            ControlCommand::ListPanes {
                session_id: "sess-123".to_string(),
            }
        );
    }

    #[test]
    fn parse_unknown_command() {
        assert!(parse_command("foobar").is_err());
    }

    #[test]
    fn parse_empty_command() {
        assert!(parse_command("").is_err());
        assert!(parse_command("  ").is_err());
    }

    #[test]
    fn format_response_block() {
        let resp = format_response(42, "{\"ok\":true}");
        assert_eq!(resp, "%begin 42\n{\"ok\":true}\n%end 42\n");
    }

    #[test]
    fn format_error_block() {
        let resp = format_error(7, "not found");
        assert_eq!(resp, "%begin 7\n%error not found\n%end 7\n");
    }

    #[test]
    fn format_session_created_notification() {
        let evt = DaemonEvent::SessionCreated {
            session_id: "sess-abc".to_string(),
        };
        assert_eq!(format_notification(&evt), "%session-changed sess-abc\n");
    }

    #[test]
    fn format_pane_output_notification() {
        let evt = DaemonEvent::PaneOutput {
            session_id: "s1".to_string(),
            pane_id: "p1".to_string(),
            data: vec![65, 66],
        };
        assert_eq!(format_notification(&evt), "%pane-output p1\n");
    }

    #[test]
    fn format_state_changed_notification() {
        let evt = DaemonEvent::StateChanged {
            old: therminal_protocol::DaemonState::Ready,
            new: therminal_protocol::DaemonState::Running,
        };
        assert_eq!(format_notification(&evt), "%state-changed ready running\n");
    }
}
