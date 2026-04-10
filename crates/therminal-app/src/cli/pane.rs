//! `therminal pane …` subcommands.

use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use clap::Subcommand;

use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use super::OutputFlags;
use super::format::{opt_i32, opt_str, tags_compact, write_json, write_tsv_row};
use super::runtime::CliCtx;

/// Validate the split ratio is in the range 0.1..=0.9.
fn parse_ratio(s: &str) -> Result<f32, String> {
    let v: f32 = s.parse().map_err(|e| format!("{e}"))?;
    if !v.is_finite() {
        return Err("ratio must be a finite number".into());
    }
    if (0.1..=0.9).contains(&v) {
        Ok(v)
    } else {
        Err("ratio must be between 0.1 and 0.9".into())
    }
}

#[derive(Subcommand, Debug)]
pub enum PaneCmd {
    /// List all panes (one record per line, TSV).
    List {
        /// Restrict to one session.
        #[arg(long)]
        session: Option<u64>,
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Create a new pane (split from an existing pane, or seed a session).
    Create {
        /// Pane id to split from. Required when `--session` is not given.
        #[arg(long)]
        from: Option<u64>,
        /// Split direction relative to the source pane. Default: vertical.
        #[arg(long, value_parser = ["vertical", "horizontal", "v", "h"])]
        split: Option<String>,
        /// Session id when seeding a new pane in an existing session.
        #[arg(long)]
        session: Option<u64>,
        /// Run this command in the new pane once the shell prompt is ready.
        #[arg(long = "spawn")]
        startup_command: Option<String>,
        /// Split ratio for the source (first) child (0.1..0.9). Default 0.5.
        #[arg(long, value_parser = parse_ratio)]
        ratio: Option<f32>,
        /// Shell binary to spawn instead of the global default (e.g. /bin/fish, powershell.exe).
        #[arg(long)]
        shell: Option<String>,
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Destroy a pane and tear down its PTY.
    Destroy { pane_id: u64 },
    /// Send raw bytes (or a key string) to a pane's PTY.
    ///
    /// Newline escapes (`\n`, `\r`, `\t`) are interpreted in `<keys>`. Use
    /// `--raw` to disable escape interpretation and forward bytes verbatim.
    Send {
        pane_id: u64,
        keys: String,
        /// Don't interpret backslash escapes in `<keys>`.
        #[arg(long)]
        raw: bool,
    },
    /// Print the last N non-empty rows of a pane's visible grid (default 10, cap 50).
    ///
    /// **Handoff (tn-8ysl → tn-sp3n)**: the `terminal.panes.get_content` MCP
    /// tool returns a `content_hash` so callers can short-circuit on
    /// unchanged screens, but `IpcResponse::PaneCaptured` does not yet
    /// carry that field. Once `content_hash` is plumbed through the
    /// IPC response, add an `--if-changed <hash>` flag here that exits 0
    /// with no output when the daemon's current hash matches.
    Peek {
        pane_id: u64,
        /// Limit to the last N non-empty rows (default 10, max 50).
        #[arg(long)]
        last: Option<usize>,
        /// Trim trailing whitespace per row.
        #[arg(long, default_value_t = true)]
        trim: bool,
        #[command(flatten)]
        out: OutputFlags,
    },
    /// Set or update a tag on a pane (`key=value`).
    Tag {
        pane_id: u64,
        /// One or more `key=value` pairs.
        kvs: Vec<String>,
    },
    /// Remove a tag from a pane.
    Untag {
        pane_id: u64,
        /// Tag keys to remove. Pass `--all` to clear every tag.
        keys: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Focus (select) a pane.
    Focus { pane_id: u64 },
    /// Move a pane to a different workspace within the same session.
    Move {
        pane_id: u64,
        /// Target workspace id (1-9).
        #[arg(long)]
        workspace: u64,
    },
    /// Swap two panes' positions in the layout tree.
    Swap { a: u64, b: u64 },
    /// Resize a pane's PTY (`<cols>x<rows>`).
    Resize { pane_id: u64, dims: String },
}

pub fn run(ctx: &CliCtx, cmd: PaneCmd) -> Result<()> {
    match cmd {
        PaneCmd::List { session, out } => list(ctx, session, out),
        PaneCmd::Create {
            from,
            split,
            session,
            startup_command,
            ratio,
            shell,
            out,
        } => create(
            ctx,
            from,
            split,
            session,
            startup_command,
            ratio,
            shell,
            out,
        ),
        PaneCmd::Destroy { pane_id } => destroy(ctx, pane_id),
        PaneCmd::Send { pane_id, keys, raw } => send(ctx, pane_id, &keys, raw),
        PaneCmd::Peek {
            pane_id,
            last,
            trim,
            out,
        } => peek(ctx, pane_id, last, trim, out),
        PaneCmd::Tag { pane_id, kvs } => tag(ctx, pane_id, &kvs),
        PaneCmd::Untag { pane_id, keys, all } => untag(ctx, pane_id, keys, all),
        PaneCmd::Focus { pane_id } => focus(ctx, pane_id),
        PaneCmd::Move { pane_id, workspace } => move_pane(ctx, pane_id, workspace),
        PaneCmd::Swap { a, b } => swap(ctx, a, b),
        PaneCmd::Resize { pane_id, dims } => resize(ctx, pane_id, &dims),
    }
}

fn list(ctx: &CliCtx, session: Option<u64>, out: OutputFlags) -> Result<()> {
    let resp = ctx.send(IpcRequest::ListPanes {
        session_id: session,
    })?;
    let panes = match resp {
        IpcResponse::Panes { panes } => panes,
        other => bail!("unexpected daemon response: {other:?}"),
    };

    if out.json {
        return write_json(&panes);
    }

    let mut stdout = io::stdout().lock();
    for p in &panes {
        // Format: pane_id<TAB>session_id<TAB>colsxrows<TAB>cwd<TAB>last_exit<TAB>agent<TAB>tags
        let dims = format!("{}x{}", p.cols, p.rows);
        write_tsv_row(
            &mut stdout,
            [
                p.pane_id.to_string().as_str(),
                p.session_id.to_string().as_str(),
                dims.as_str(),
                opt_str(&p.cwd),
                opt_i32(p.last_exit_code).as_str(),
                opt_str(&p.agent_name),
                tags_compact(&p.tags).as_str(),
            ],
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create(
    ctx: &CliCtx,
    from: Option<u64>,
    split: Option<String>,
    session: Option<u64>,
    startup_command: Option<String>,
    ratio: Option<f32>,
    shell: Option<String>,
    out: OutputFlags,
) -> Result<()> {
    // Determine the source pane to split from. If `--from` is set we use it
    // directly; otherwise we look up the first pane in the requested session
    // (or the only session, if there's exactly one). When no panes exist
    // yet we create a session first.
    let source_pane = match from {
        Some(p) => p,
        None => find_seed_pane(ctx, session, shell.as_deref())?,
    };

    let horizontal = matches!(split.as_deref(), Some("horizontal" | "h"));

    let resp = ctx.send(IpcRequest::SplitPane {
        pane_id: source_pane,
        horizontal,
        cwd: None,
        startup_command,
        ratio,
        shell,
    })?;
    let new_pane = match resp {
        IpcResponse::PaneSplit { new_pane_id } => new_pane_id,
        other => bail!("unexpected daemon response: {other:?}"),
    };

    if out.json {
        write_json(&serde_json::json!({ "pane_id": new_pane }))
    } else {
        println!("{new_pane}");
        Ok(())
    }
}

/// Find a pane to split from when the user did not pass `--from`.
///
/// Strategy:
/// 1. If `--session N` is set, list panes filtered to that session and pick
///    the first one. If the session is empty, error out.
/// 2. Otherwise list all panes; if there's at least one, return its id.
/// 3. If the daemon has no panes at all, create a session (which spawns a
///    seed pane) and return that pane's id.
fn find_seed_pane(ctx: &CliCtx, session: Option<u64>, shell: Option<&str>) -> Result<u64> {
    let resp = ctx.send(IpcRequest::ListPanes {
        session_id: session,
    })?;
    let panes = match resp {
        IpcResponse::Panes { panes } => panes,
        other => bail!("unexpected daemon response: {other:?}"),
    };
    if let Some(first) = panes.first() {
        return Ok(first.pane_id);
    }
    if session.is_some() {
        bail!("session has no panes — pass --from <pane_id> explicitly");
    }
    // No panes at all → spin up a session.
    let resp = ctx.send(IpcRequest::CreateSession {
        name: None,
        cols: None,
        rows: None,
        shell: shell.map(str::to_string),
    })?;
    let session_id = match resp {
        IpcResponse::SessionCreated { session_id } => session_id,
        other => bail!("unexpected daemon response: {other:?}"),
    };
    let resp = ctx.send(IpcRequest::ListPanes {
        session_id: Some(session_id),
    })?;
    let panes = match resp {
        IpcResponse::Panes { panes } => panes,
        other => bail!("unexpected daemon response: {other:?}"),
    };
    panes
        .first()
        .map(|p| p.pane_id)
        .context("freshly created session has no panes — daemon bug")
}

fn destroy(ctx: &CliCtx, pane_id: u64) -> Result<()> {
    let resp = ctx.send(IpcRequest::KillPane { pane_id })?;
    match resp {
        IpcResponse::PaneKilled { pane_id } => {
            println!("{pane_id}");
            Ok(())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn send(ctx: &CliCtx, pane_id: u64, keys: &str, raw: bool) -> Result<()> {
    let bytes: Vec<u8> = if raw {
        keys.as_bytes().to_vec()
    } else {
        interpret_escapes(keys)
    };
    let resp = ctx.send(IpcRequest::SendKeys {
        pane_id,
        keys: bytes,
    })?;
    match resp {
        IpcResponse::KeysSent { .. } => Ok(()),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn peek(
    ctx: &CliCtx,
    pane_id: u64,
    last: Option<usize>,
    trim: bool,
    out: OutputFlags,
) -> Result<()> {
    let resp = ctx.send(IpcRequest::CapturePane { pane_id })?;
    let (lines, cursor_col, cursor_line, cols, rows) = match resp {
        IpcResponse::PaneCaptured {
            lines,
            cursor_col,
            cursor_line,
            cols,
            rows,
            ..
        } => (lines, cursor_col, cursor_line, cols, rows),
        other => bail!("unexpected daemon response: {other:?}"),
    };

    let mut processed: Vec<String> = if trim {
        lines
            .into_iter()
            .map(|l| l.trim_end().to_string())
            .collect()
    } else {
        lines
    };

    // Default to 10 non-empty lines (matches MCP terminal.panes.peek default).
    // Cap at 50 to mirror the server-side cap the MCP tool applies.
    let n = last.unwrap_or(10).min(50);
    {
        // Drop fully empty trailing rows so the result contains content lines,
        // not blank padding rows. Then keep the last `n` non-empty lines.
        while matches!(processed.last(), Some(s) if s.is_empty()) {
            processed.pop();
        }
        if processed.len() > n {
            let drop = processed.len() - n;
            processed.drain(0..drop);
        }
    }

    if out.json {
        return write_json(&serde_json::json!({
            "pane_id": pane_id,
            "cols": cols,
            "rows": rows,
            "cursor_col": cursor_col,
            "cursor_line": cursor_line,
            "lines": processed,
        }));
    }

    let mut stdout = io::stdout().lock();
    for line in &processed {
        stdout.write_all(line.as_bytes())?;
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

fn tag(ctx: &CliCtx, pane_id: u64, kvs: &[String]) -> Result<()> {
    if kvs.is_empty() {
        bail!("expected one or more key=value arguments");
    }
    let mut tags = std::collections::HashMap::new();
    for kv in kvs {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("expected key=value, got {kv:?}"))?;
        tags.insert(k.to_string(), v.to_string());
    }
    let resp = ctx.send(IpcRequest::TagPane { pane_id, tags })?;
    match resp {
        IpcResponse::PaneTagged { tags, .. } => {
            println!("{}", tags_compact(&tags));
            Ok(())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn untag(ctx: &CliCtx, pane_id: u64, keys: Vec<String>, all: bool) -> Result<()> {
    let req = if all {
        IpcRequest::UntagPane {
            pane_id,
            keys: None,
        }
    } else {
        if keys.is_empty() {
            bail!("expected at least one key to remove (or --all)");
        }
        IpcRequest::UntagPane {
            pane_id,
            keys: Some(keys),
        }
    };
    let resp = ctx.send(req)?;
    match resp {
        IpcResponse::PaneTagged { tags, .. } => {
            println!("{}", tags_compact(&tags));
            Ok(())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn focus(ctx: &CliCtx, pane_id: u64) -> Result<()> {
    let resp = ctx.send(IpcRequest::SelectPane { pane_id })?;
    match resp {
        IpcResponse::PaneSelected { pane_id } => {
            println!("{pane_id}");
            Ok(())
        }
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn move_pane(ctx: &CliCtx, pane_id: u64, workspace: u64) -> Result<()> {
    let resp = ctx.send(IpcRequest::MovePane {
        pane_id,
        target_workspace_id: workspace,
    })?;
    match resp {
        IpcResponse::PaneMoved {
            pane_id,
            target_workspace_id,
            ..
        } => {
            println!("{pane_id}\t{target_workspace_id}");
            Ok(())
        }
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn swap(ctx: &CliCtx, a: u64, b: u64) -> Result<()> {
    let resp = ctx.send(IpcRequest::SwapPane { a, b })?;
    match resp {
        IpcResponse::PaneSwapped { a, b } => {
            println!("{a}\t{b}");
            Ok(())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

fn resize(ctx: &CliCtx, pane_id: u64, dims: &str) -> Result<()> {
    let (c, r) = dims
        .split_once(['x', 'X'])
        .with_context(|| format!("expected <cols>x<rows>, got {dims:?}"))?;
    let cols: u16 = c.parse().context("invalid cols")?;
    let rows: u16 = r.parse().context("invalid rows")?;
    let resp = ctx.send(IpcRequest::ResizePane {
        pane_id,
        cols,
        rows,
    })?;
    match resp {
        IpcResponse::PaneResized { cols, rows, .. } => {
            println!("{cols}x{rows}");
            Ok(())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

/// Tiny escape interpreter for `pane send` — supports the four escapes you
/// reach for from a shell-quoted argument: `\n`, `\r`, `\t`, `\\`. Anything
/// else passes through verbatim so users don't have to think about it.
fn interpret_escapes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push(b'\n'),
                Some('r') => out.push(b'\r'),
                Some('t') => out.push(b'\t'),
                Some('\\') => out.push(b'\\'),
                Some(other) => {
                    out.push(b'\\');
                    out.extend_from_slice(other.encode_utf8(&mut [0; 4]).as_bytes());
                }
                None => out.push(b'\\'),
            }
        } else {
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::interpret_escapes;

    #[test]
    fn escapes_basic() {
        assert_eq!(interpret_escapes("hello"), b"hello");
        assert_eq!(interpret_escapes("hi\\n"), b"hi\n");
        assert_eq!(interpret_escapes("a\\tb\\rc\\\\d"), b"a\tb\rc\\d");
    }

    #[test]
    fn escapes_unknown_passes_through() {
        assert_eq!(interpret_escapes("\\q"), b"\\q");
    }

    #[test]
    fn escapes_trailing_backslash() {
        assert_eq!(interpret_escapes("foo\\"), b"foo\\");
    }
}
