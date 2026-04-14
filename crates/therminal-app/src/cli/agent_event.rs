//! `therminal agent-event …` subcommands.
//!
//! Pushes structured hook signals to the daemon so the harness broadcast
//! channel receives lifecycle events without relying on file-system polling.
//! Primary consumer: Claude Code hook scripts calling from WSL when the
//! daemon runs as a Windows native process.

use anyhow::{Result, bail};
use clap::Subcommand;

use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use super::runtime::CliCtx;

#[derive(Subcommand, Debug)]
pub enum AgentEventCmd {
    /// Push a hook signal to the daemon.
    ///
    /// Constructs a HookSignal from the provided flags and sends it as
    /// `IpcRequest::PushAgentEvent`. The daemon dispatches the signal to
    /// the harness broadcast channel and (for subagent events) emits
    /// `DaemonEvent::SubagentStarted` / `SubagentStopped`.
    Push {
        /// Hook event name: session_start, session_stop, tool_state,
        /// subagent_start, subagent_stop, stop_failure.
        #[arg(long)]
        event: String,

        /// Claude Code session UUID ($CLAUDE_SESSION_ID).
        #[arg(long)]
        session_id: Option<String>,

        /// Parent session UUID (required for subagent_start / subagent_stop).
        #[arg(long)]
        parent_session_id: Option<String>,

        /// Per-subagent identifier (agent_id from Claude Code).
        #[arg(long)]
        agent_id: Option<String>,

        /// Agent type string (for subagent_start).
        #[arg(long)]
        agent_type: Option<String>,

        /// Project directory (for session_start).
        #[arg(long)]
        project_dir: Option<String>,

        /// PID of the Claude Code process (for session_start).
        #[arg(long)]
        pid: Option<u32>,

        /// Tool name (for tool_state).
        #[arg(long)]
        tool_name: Option<String>,

        /// Tool status: tool_use, idle, processing (for tool_state).
        #[arg(long)]
        status: Option<String>,

        /// Brief tool input summary (for tool_state).
        #[arg(long)]
        tool_input_summary: Option<String>,

        /// Stop reason (for session_stop).
        #[arg(long)]
        reason: Option<String>,

        /// Error type (for stop_failure).
        #[arg(long)]
        error_type: Option<String>,
    },
}

pub fn run(ctx: &CliCtx, cmd: AgentEventCmd) -> Result<()> {
    match cmd {
        AgentEventCmd::Push {
            event,
            session_id,
            parent_session_id,
            agent_id,
            agent_type,
            project_dir,
            pid,
            tool_name,
            status,
            tool_input_summary,
            reason,
            error_type,
        } => {
            // Build the HookSignal JSON payload. The daemon deserialises this
            // into `therminal_harness_claude::HookSignal`.
            let signal = serde_json::json!({
                "session_id": session_id.unwrap_or_default(),
                "event": event,
                "project_dir": project_dir,
                "pid": pid,
                "reason": reason,
                "status": status,
                "tool_name": tool_name,
                "tool_input_summary": tool_input_summary,
                "agent_id": agent_id,
                "agent_type": agent_type,
                "parent_session_id": parent_session_id,
                "error_type": error_type,
            });

            let resp = ctx.send(IpcRequest::PushAgentEvent { signal })?;
            match resp {
                IpcResponse::AgentEventPushed => Ok(()),
                other => bail!("unexpected response: {other:?}"),
            }
        }
    }
}
