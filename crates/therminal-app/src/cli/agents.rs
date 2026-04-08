//! `therminal agents …` subcommands.

use anyhow::{Result, bail};
use clap::Subcommand;

use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use super::OutputFlags;
use super::format::{opt_str, write_json, write_tsv_row};
use super::runtime::CliCtx;

#[derive(Subcommand, Debug)]
pub enum AgentsCmd {
    /// List detected agents.
    List {
        /// Filter to a single pane.
        #[arg(long)]
        pane: Option<u64>,
        #[command(flatten)]
        out: OutputFlags,
    },
}

pub fn run(ctx: &CliCtx, cmd: AgentsCmd) -> Result<()> {
    match cmd {
        AgentsCmd::List { pane, out } => list(ctx, pane, out),
    }
}

fn list(ctx: &CliCtx, pane: Option<u64>, out: OutputFlags) -> Result<()> {
    let resp = ctx.send(IpcRequest::ListAgents)?;
    let mut agents = match resp {
        IpcResponse::Agents { agents } => agents,
        other => bail!("unexpected daemon response: {other:?}"),
    };
    if let Some(p) = pane {
        agents.retain(|a| a.pane_id == p);
    }
    if out.json {
        return write_json(&agents);
    }
    let mut stdout = std::io::stdout().lock();
    for a in &agents {
        // Format: pane_id<TAB>agent_type<TAB>status<TAB>name<TAB>current_tool<TAB>pid
        write_tsv_row(
            &mut stdout,
            [
                a.pane_id.to_string().as_str(),
                a.agent_type.as_str(),
                a.status.as_str(),
                a.name.as_str(),
                opt_str(&a.current_tool),
                a.pid.map(|p| p.to_string()).unwrap_or_default().as_str(),
            ],
        )?;
    }
    Ok(())
}
