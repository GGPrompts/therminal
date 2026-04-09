//! `therminal semantic …` subcommands.
//!
//! Hotspot detection runs **client-side** in the CLI: the daemon's
//! `terminal_terminal::hotspot_detection` library is the same code path
//! the GUI and the MCP server use, so the CLI just calls `CapturePane` and
//! re-runs the regex pass locally. That keeps the daemon IPC surface
//! unchanged while still giving callers the answer.
//!
//! Command-history (OSC 633) is daemon-side state — the parser lives in
//! the PTY reader thread. tn-8ysl added the `IpcRequest::QueryCommands`
//! primitive so the CLI can fetch the same data the
//! `terminal.semantic.query_commands` MCP tool returns without paying JSON-RPC
//! framing costs.

use anyhow::{Result, bail};
use clap::Subcommand;

use therminal_protocol::daemon::{CommandSummary, IpcRequest, IpcResponse};
use therminal_terminal::hotspot_detection::{HotspotKind, detect_hotspots_from_text};

use super::OutputFlags;
use super::format::{write_json, write_tsv_row};
use super::runtime::CliCtx;

#[derive(Subcommand, Debug)]
pub enum SemanticCmd {
    /// Recent shell commands captured via OSC 633 from a pane.
    Commands {
        pane_id: u64,
        /// Drop blocks whose `start_line` is below this value.
        #[arg(long, default_value_t = 0)]
        since_line: usize,
        /// Keep only the newest N entries. Capped daemon-side.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Detected hotspots (file paths, URLs, error locations, …) in a pane.
    ///
    /// Runs client-side over `CapturePane` output, so the daemon path is
    /// just the existing `IpcRequest::CapturePane` plus a local regex pass.
    Hotspots {
        pane_id: u64,
        /// Filter to one hotspot kind (file, url, git_ref, issue).
        #[arg(long)]
        kind: Option<String>,
        #[command(flatten)]
        out: OutputFlags,
    },
}

pub fn run(ctx: &CliCtx, cmd: SemanticCmd) -> Result<()> {
    match cmd {
        SemanticCmd::Commands {
            pane_id,
            since_line,
            limit,
            out,
        } => commands(ctx, pane_id, since_line, limit, out),
        SemanticCmd::Hotspots { pane_id, kind, out } => {
            hotspots(ctx, pane_id, kind.as_deref(), out)
        }
    }
}

fn commands(
    ctx: &CliCtx,
    pane_id: u64,
    since_line: usize,
    limit: usize,
    out: OutputFlags,
) -> Result<()> {
    let resp = ctx.send(IpcRequest::QueryCommands {
        pane_id,
        since_line,
        limit,
    })?;
    let commands: Vec<CommandSummary> = match resp {
        IpcResponse::Commands { commands, .. } => commands,
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    };

    if out.json {
        let rows: Vec<_> = commands
            .iter()
            .map(|c| {
                serde_json::json!({
                    "command": c.command,
                    "exit_code": c.exit_code,
                    "duration_ms": c.duration_ms,
                    "start_line": c.start_line,
                    "end_line": c.end_line,
                    "started_at_secs": c.started_at_secs,
                })
            })
            .collect();
        return write_json(&rows);
    }

    let mut stdout = std::io::stdout().lock();
    for c in &commands {
        // Format: start_line<TAB>exit_code<TAB>duration_ms<TAB>command
        let exit = c
            .exit_code
            .map(|e| e.to_string())
            .unwrap_or_else(|| "-".to_string());
        let dur = c
            .duration_ms
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".to_string());
        let text = c.command.as_deref().unwrap_or("");
        write_tsv_row(
            &mut stdout,
            [
                c.start_line.to_string().as_str(),
                exit.as_str(),
                dur.as_str(),
                text,
            ],
        )?;
    }
    Ok(())
}

fn hotspots(ctx: &CliCtx, pane_id: u64, kind_filter: Option<&str>, out: OutputFlags) -> Result<()> {
    let resp = ctx.send(IpcRequest::CapturePane { pane_id })?;
    let lines = match resp {
        IpcResponse::PaneCaptured { lines, .. } => lines,
        other => bail!("unexpected daemon response: {other:?}"),
    };

    let mut hotspots = detect_hotspots_from_text(&lines);
    if let Some(filter) = kind_filter {
        hotspots.retain(|h| h.kind.as_str() == filter || kind_matches(&h.kind, filter));
    }

    if out.json {
        let rows: Vec<_> = hotspots
            .iter()
            .map(|h| {
                serde_json::json!({
                    "kind": h.kind.as_str(),
                    "text": h.text,
                    "row": h.row,
                    "col_start": h.start_col,
                    "col_end": h.end_col,
                })
            })
            .collect();
        return write_json(&rows);
    }

    let mut stdout = std::io::stdout().lock();
    for h in &hotspots {
        // Format: kind<TAB>row<TAB>col_start<TAB>col_end<TAB>text
        write_tsv_row(
            &mut stdout,
            [
                h.kind.as_str(),
                h.row.to_string().as_str(),
                h.start_col.to_string().as_str(),
                h.end_col.to_string().as_str(),
                h.text.as_str(),
            ],
        )?;
    }
    Ok(())
}

fn kind_matches(k: &HotspotKind, want: &str) -> bool {
    matches!(
        (k, want),
        (
            HotspotKind::FilePath | HotspotKind::ErrorLocation,
            "file" | "filepath"
        ) | (HotspotKind::Url, "url")
            | (HotspotKind::GitRef, "git_ref" | "gitref" | "git")
            | (HotspotKind::IssueRef, "issue" | "issue_ref"),
    )
}
