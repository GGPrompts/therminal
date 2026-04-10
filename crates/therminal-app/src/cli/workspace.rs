//! `therminal workspace …` subcommands.
//!
//! tn-8ysl added the `IpcRequest::SwitchWorkspace` primitive so the CLI
//! can change the active workspace without going through the GUI. The
//! daemon updates `Session::active_workspace` via
//! `SessionManager::set_active_workspace` and broadcasts a
//! `WorkspaceChanged` event so subscribed clients (including the GUI)
//! see the change.

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
    Switch {
        #[arg(long)]
        session: u64,
        workspace_id: u64,
    },
    /// Create a new empty workspace in a session.
    Create {
        #[arg(long)]
        session: u64,
        /// Human-readable workspace name (e.g. "build", "logs").
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Rename an existing workspace.
    Rename {
        #[arg(long)]
        session: u64,
        workspace_id: u64,
        name: String,
    },
}

pub fn run(ctx: &CliCtx, cmd: WorkspaceCmd) -> Result<()> {
    match cmd {
        WorkspaceCmd::List { session, out } => list(ctx, session, out),
        WorkspaceCmd::Switch {
            session,
            workspace_id,
        } => switch(ctx, session, workspace_id),
        WorkspaceCmd::Create { session, name, out } => create(ctx, session, name, out),
        WorkspaceCmd::Rename {
            session,
            workspace_id,
            name,
        } => rename(ctx, session, workspace_id, &name),
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

fn switch(ctx: &CliCtx, session: u64, workspace_id: u64) -> Result<()> {
    let resp = ctx.send(IpcRequest::SwitchWorkspace {
        session_id: session,
        workspace_id,
    })?;
    match resp {
        IpcResponse::WorkspaceSwitched { .. } => Ok(()),
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn create(ctx: &CliCtx, session: u64, name: Option<String>, out: OutputFlags) -> Result<()> {
    let resp = ctx.send(IpcRequest::CreateWorkspace {
        session_id: session,
        name,
    })?;
    match resp {
        IpcResponse::WorkspaceCreated {
            workspace_id,
            session_id,
        } => {
            if out.json {
                write_json(&serde_json::json!({
                    "session_id": session_id,
                    "workspace_id": workspace_id,
                }))
            } else {
                println!("{workspace_id}");
                Ok(())
            }
        }
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn rename(ctx: &CliCtx, session: u64, workspace_id: u64, name: &str) -> Result<()> {
    let resp = ctx.send(IpcRequest::RenameWorkspace {
        session_id: session,
        workspace_id,
        name: name.to_string(),
    })?;
    match resp {
        IpcResponse::WorkspaceRenamed { .. } => Ok(()),
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}
