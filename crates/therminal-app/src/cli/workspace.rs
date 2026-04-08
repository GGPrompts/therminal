//! `therminal workspace …` subcommands.
//!
//! Workspace **switching** is a GUI-only operation today (the daemon stores
//! the topology but doesn't drive a focus change without a client). Until
//! the daemon grows a `SwitchWorkspace` IPC primitive (tracked separately),
//! `workspace switch` is a stub that prints a clear error so scripts notice
//! the gap rather than silently succeed.

use anyhow::{Result, bail};
use clap::Subcommand;

use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use super::OutputFlags;
use super::format::{write_json, write_tsv_row};
use super::runtime::CliCtx;

#[derive(Subcommand, Debug)]
pub enum WorkspaceCmd {
    /// List workspaces, optionally restricted to one session.
    List {
        #[arg(long)]
        session: Option<u64>,
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Switch the active workspace within a session.
    ///
    /// Currently a no-op stub: workspace switching is GUI-driven and the
    /// daemon does not yet expose a switch RPC.
    Switch {
        #[arg(long)]
        session: u64,
        workspace_id: u64,
    },
}

pub fn run(ctx: &CliCtx, cmd: WorkspaceCmd) -> Result<()> {
    match cmd {
        WorkspaceCmd::List { session, out } => list(ctx, session, out),
        WorkspaceCmd::Switch {
            session,
            workspace_id,
        } => switch(ctx, session, workspace_id),
    }
}

fn list(ctx: &CliCtx, session: Option<u64>, out: OutputFlags) -> Result<()> {
    // The daemon's GetWorkspaces IPC is per-session; without a session id we
    // walk every session via ListSessions first.
    let session_ids: Vec<u64> = match session {
        Some(s) => vec![s],
        None => match ctx.send(IpcRequest::ListSessions)? {
            IpcResponse::Sessions { session_ids } => session_ids,
            other => bail!("unexpected daemon response: {other:?}"),
        },
    };

    if out.json {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for sid in &session_ids {
            match ctx.send(IpcRequest::GetWorkspaces { session_id: *sid })? {
                IpcResponse::Workspaces {
                    workspaces,
                    active_workspace,
                    ..
                } => {
                    for ws in workspaces {
                        rows.push(serde_json::json!({
                            "session_id": sid,
                            "workspace_id": ws.id,
                            "name": ws.name,
                            "pane_count": ws.pane_ids.len(),
                            "is_active": ws.id == active_workspace,
                            "pane_ids": ws.pane_ids,
                        }));
                    }
                }
                other => bail!("unexpected daemon response: {other:?}"),
            }
        }
        return write_json(&rows);
    }

    let mut stdout = std::io::stdout().lock();
    for sid in &session_ids {
        let (workspaces, active_workspace) =
            match ctx.send(IpcRequest::GetWorkspaces { session_id: *sid })? {
                IpcResponse::Workspaces {
                    workspaces,
                    active_workspace,
                    ..
                } => (workspaces, active_workspace),
                other => bail!("unexpected daemon response: {other:?}"),
            };
        for ws in &workspaces {
            // Format: session_id<TAB>workspace_id<TAB>name<TAB>pane_count<TAB>active(0|1)
            let active = if ws.id == active_workspace { "1" } else { "0" };
            write_tsv_row(
                &mut stdout,
                [
                    sid.to_string().as_str(),
                    ws.id.to_string().as_str(),
                    ws.name.as_str(),
                    ws.pane_ids.len().to_string().as_str(),
                    active,
                ],
            )?;
        }
    }
    Ok(())
}

fn switch(_ctx: &CliCtx, session: u64, workspace_id: u64) -> Result<()> {
    bail!(
        "workspace switch is GUI-driven; daemon has no SwitchWorkspace RPC yet (session={session}, workspace={workspace_id})"
    )
}
