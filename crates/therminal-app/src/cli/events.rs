//! `therminal events …` subcommands.
//!
//! Streams `DaemonEvent`s as one JSON document per line. Even though the
//! rest of the CLI prefers TSV, an event stream is naturally heterogeneous
//! (different variants carry different fields), so JSON Lines is the right
//! shape: each event is still ~100 bytes and trivially `jq`-friendly.

use anyhow::{Result, bail};
use clap::Args;

use therminal_protocol::daemon::{DaemonEvent, EventKind, IpcRequest, IpcResponse};

use super::runtime::CliCtx;

#[derive(Args, Debug)]
pub struct EventsArgs {
    /// Stream events until interrupted (Ctrl+C). Default behaviour.
    #[arg(long)]
    pub follow: bool,
    /// Comma-separated event kinds to subscribe to. Empty = all kinds.
    /// Valid: state_changed, session_created, session_destroyed,
    /// pane_output, workspace_changed, pane_exited, pane_resized.
    #[arg(long, value_delimiter = ',')]
    pub kinds: Vec<String>,
    /// Comma-separated pane ids; events for other panes are dropped.
    #[arg(long, value_delimiter = ',')]
    pub panes: Vec<u64>,
    /// Maximum number of events to print before exiting (handy for tests).
    #[arg(long)]
    pub limit: Option<usize>,
}

pub fn run(ctx: &CliCtx, args: EventsArgs) -> Result<()> {
    let kinds = parse_kinds(&args.kinds)?;

    // Subscribe.
    let resp = ctx.send(IpcRequest::Subscribe {
        filter: kinds.clone(),
    })?;
    if !matches!(resp, IpcResponse::Subscribed { .. }) {
        bail!("unexpected daemon response to Subscribe: {resp:?}");
    }

    let pane_filter: Option<std::collections::HashSet<u64>> = if args.panes.is_empty() {
        None
    } else {
        Some(args.panes.into_iter().collect())
    };

    let mut printed = 0usize;
    let limit = args.limit.unwrap_or(usize::MAX);

    // Drive the event loop on the existing runtime.
    let client = ctx.client.clone();
    ctx.rt.block_on(async {
        loop {
            let Some(event) = client.recv_event().await else {
                break;
            };
            if !pane_matches(&event, pane_filter.as_ref()) {
                continue;
            }
            // Emit one JSON line per event.
            let line = match serde_json::to_string(&event) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("therminal: failed to encode event: {e}");
                    continue;
                }
            };
            println!("{line}");
            printed += 1;
            if printed >= limit {
                break;
            }
        }
    });
    Ok(())
}

fn parse_kinds(raw: &[String]) -> Result<Vec<EventKind>> {
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        let kind = match s.as_str() {
            "state_changed" => EventKind::StateChanged,
            "session_created" => EventKind::SessionCreated,
            "session_destroyed" => EventKind::SessionDestroyed,
            "pane_output" => EventKind::PaneOutput,
            "workspace_changed" => EventKind::WorkspaceChanged,
            "pane_exited" => EventKind::PaneExited,
            "pane_resized" => EventKind::PaneResized,
            other => bail!("unknown event kind: {other}"),
        };
        out.push(kind);
    }
    Ok(out)
}

fn pane_matches(event: &DaemonEvent, panes: Option<&std::collections::HashSet<u64>>) -> bool {
    let Some(set) = panes else {
        return true;
    };
    match event {
        DaemonEvent::PaneOutput { pane_id, .. }
        | DaemonEvent::PaneExited { pane_id, .. }
        | DaemonEvent::PaneResized { pane_id, .. } => set.contains(pane_id),
        // Non-pane-scoped events pass through unconditionally so callers can
        // see things like SessionCreated even when filtering by pane.
        _ => true,
    }
}
