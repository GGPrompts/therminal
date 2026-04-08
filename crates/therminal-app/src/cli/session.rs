//! `therminal session …` subcommands.

use anyhow::{Result, bail};
use clap::Subcommand;

use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use super::OutputFlags;
use super::format::{write_json, write_tsv_row};
use super::runtime::CliCtx;

#[derive(Subcommand, Debug)]
pub enum SessionCmd {
    /// List active session IDs.
    List {
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Create a new session, optionally with a name.
    Create {
        /// Optional human-readable name (e.g. "build", "logs").
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Destroy a session and all its panes.
    Destroy { session_id: u64 },
}

pub fn run(ctx: &CliCtx, cmd: SessionCmd) -> Result<()> {
    match cmd {
        SessionCmd::List { out } => list(ctx, out),
        SessionCmd::Create { name, out } => create(ctx, name, out),
        SessionCmd::Destroy { session_id } => destroy(ctx, session_id),
    }
}

fn list(ctx: &CliCtx, out: OutputFlags) -> Result<()> {
    let resp = ctx.send(IpcRequest::ListSessions)?;
    let session_ids = match resp {
        IpcResponse::Sessions { session_ids } => session_ids,
        other => bail!("unexpected daemon response: {other:?}"),
    };
    if out.json {
        return write_json(&serde_json::json!({ "session_ids": session_ids }));
    }
    let mut stdout = std::io::stdout().lock();
    for sid in &session_ids {
        write_tsv_row(&mut stdout, [sid.to_string().as_str()])?;
    }
    Ok(())
}

fn create(ctx: &CliCtx, name: Option<String>, out: OutputFlags) -> Result<()> {
    let resp = ctx.send(IpcRequest::CreateSession { name })?;
    let session_id = match resp {
        IpcResponse::SessionCreated { session_id } => session_id,
        other => bail!("unexpected daemon response: {other:?}"),
    };
    if out.json {
        write_json(&serde_json::json!({ "session_id": session_id }))
    } else {
        println!("{session_id}");
        Ok(())
    }
}

fn destroy(ctx: &CliCtx, session_id: u64) -> Result<()> {
    let resp = ctx.send(IpcRequest::DestroySession { session_id })?;
    match resp {
        IpcResponse::SessionDestroyed { session_id } => {
            println!("{session_id}");
            Ok(())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}
