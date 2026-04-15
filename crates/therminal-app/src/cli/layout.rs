//! `therminal layout …` subcommands.
//!
//! tn-j3ke: `layout batch` reads newline-delimited commands from stdin,
//! parses each into an `IpcRequest`, wraps them in `BatchLayoutOps`, and
//! sends as one atomic IPC call. This eliminates intermediate redraws
//! when scripting complex layout setups.

use std::io::{self, BufRead};

use anyhow::{Result, bail};
use clap::Subcommand;

use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use super::OutputFlags;
use super::format::write_json;
use super::runtime::CliCtx;

#[derive(Subcommand, Debug)]
pub enum LayoutCmd {
    /// Execute multiple layout operations atomically (zero intermediate redraws).
    ///
    /// Reads newline-delimited commands from stdin. Each line is a
    /// space-separated command matching the existing CLI surface:
    ///
    ///   split <pane_id> [--horizontal] [--ratio 0.6] [--spawn 'cmd']
    ///   kill <pane_id>
    ///   focus <pane_id>
    ///   swap <pane_a> <pane_b>
    ///   move <pane_id> <workspace_id>
    ///   create-workspace <session_id> [name]
    ///   switch-workspace <session_id> <workspace_id>
    ///   rename-workspace <session_id> <workspace_id> <name>
    ///
    /// Returns one JSON result per operation on stdout.
    Batch {
        #[command(flatten)]
        out: OutputFlags,
    },
}

pub fn run(ctx: &CliCtx, cmd: LayoutCmd) -> Result<()> {
    match cmd {
        LayoutCmd::Batch { out } => batch(ctx, out),
    }
}

/// Parse a single line into an `IpcRequest`.
fn parse_line(line: &str) -> Result<IpcRequest> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        bail!("empty or comment line");
    }

    // Tokenize respecting single-quoted strings.
    let tokens = shell_tokenize(line);
    if tokens.is_empty() {
        bail!("empty command");
    }

    let cmd = tokens[0].as_str();
    let args = &tokens[1..];

    match cmd {
        "split" => parse_split(args),
        "kill" => parse_kill(args),
        "focus" => parse_focus(args),
        "swap" => parse_swap(args),
        "move" => parse_move(args),
        "create-workspace" => parse_create_workspace(args),
        "switch-workspace" => parse_switch_workspace(args),
        "rename-workspace" => parse_rename_workspace(args),
        _ => bail!("unknown batch command: {cmd}"),
    }
}

fn parse_split(args: &[String]) -> Result<IpcRequest> {
    if args.is_empty() {
        bail!("split requires <pane_id>");
    }
    let pane_id: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad pane_id: {e}"))?;
    let mut horizontal = false;
    let mut ratio: Option<f32> = None;
    let mut spawn: Option<String> = None;
    let mut shell: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut worktree: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--horizontal" | "-h" => horizontal = true,
            "--vertical" | "-v" => horizontal = false,
            "--ratio" => {
                i += 1;
                ratio = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--ratio requires a value"))?
                        .parse()
                        .map_err(|e| anyhow::anyhow!("bad ratio: {e}"))?,
                );
            }
            "--spawn" => {
                i += 1;
                spawn = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--spawn requires a value"))?
                        .clone(),
                );
            }
            "--shell" => {
                i += 1;
                shell = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--shell requires a value"))?
                        .clone(),
                );
            }
            "--cwd" => {
                i += 1;
                cwd = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--cwd requires a value"))?
                        .clone(),
                );
            }
            "--worktree" => {
                i += 1;
                worktree = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--worktree requires a value"))?
                        .clone(),
                );
            }
            other => bail!("unknown split flag: {other}"),
        }
        i += 1;
    }

    Ok(IpcRequest::SplitPane {
        pane_id,
        horizontal,
        cwd,
        startup_command: spawn,
        ratio,
        shell,
        worktree,
        profile: None,
    })
}

fn parse_kill(args: &[String]) -> Result<IpcRequest> {
    if args.is_empty() {
        bail!("kill requires <pane_id>");
    }
    let pane_id: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad pane_id: {e}"))?;
    Ok(IpcRequest::KillPane { pane_id })
}

fn parse_focus(args: &[String]) -> Result<IpcRequest> {
    if args.is_empty() {
        bail!("focus requires <pane_id>");
    }
    let pane_id: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad pane_id: {e}"))?;
    Ok(IpcRequest::SelectPane { pane_id })
}

fn parse_swap(args: &[String]) -> Result<IpcRequest> {
    if args.len() < 2 {
        bail!("swap requires <pane_a> <pane_b>");
    }
    let a: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad pane_a: {e}"))?;
    let b: u64 = args[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad pane_b: {e}"))?;
    Ok(IpcRequest::SwapPane { a, b })
}

fn parse_move(args: &[String]) -> Result<IpcRequest> {
    if args.len() < 2 {
        bail!("move requires <pane_id> <workspace_id>");
    }
    let pane_id: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad pane_id: {e}"))?;
    let target_workspace_id: u64 = args[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad workspace_id: {e}"))?;
    Ok(IpcRequest::MovePane {
        pane_id,
        target_workspace_id,
    })
}

fn parse_create_workspace(args: &[String]) -> Result<IpcRequest> {
    if args.is_empty() {
        bail!("create-workspace requires <session_id>");
    }
    let session_id: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad session_id: {e}"))?;
    let name = args.get(1).cloned();
    Ok(IpcRequest::CreateWorkspace { session_id, name })
}

fn parse_switch_workspace(args: &[String]) -> Result<IpcRequest> {
    if args.len() < 2 {
        bail!("switch-workspace requires <session_id> <workspace_id>");
    }
    let session_id: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad session_id: {e}"))?;
    let workspace_id: u64 = args[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad workspace_id: {e}"))?;
    Ok(IpcRequest::SwitchWorkspace {
        session_id,
        workspace_id,
    })
}

fn parse_rename_workspace(args: &[String]) -> Result<IpcRequest> {
    if args.len() < 3 {
        bail!("rename-workspace requires <session_id> <workspace_id> <name>");
    }
    let session_id: u64 = args[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad session_id: {e}"))?;
    let workspace_id: u64 = args[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("bad workspace_id: {e}"))?;
    let name = args[2].clone();
    Ok(IpcRequest::RenameWorkspace {
        session_id,
        workspace_id,
        name,
    })
}

/// Simple shell-like tokenizer that respects single-quoted strings.
fn shell_tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    for ch in input.chars() {
        match ch {
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn batch(ctx: &CliCtx, out: OutputFlags) -> Result<()> {
    let stdin = io::stdin().lock();
    let mut ops = Vec::new();
    let mut line_num = 0usize;

    for line in stdin.lines() {
        line_num += 1;
        let line = line?;
        let trimmed = line.trim();
        // Skip blank lines and comments.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match parse_line(trimmed) {
            Ok(req) => ops.push(req),
            Err(e) => bail!("line {line_num}: {e}"),
        }
    }

    if ops.is_empty() {
        bail!("no operations to execute (stdin was empty)");
    }

    let resp = ctx.send(IpcRequest::BatchLayoutOps { ops })?;
    match resp {
        IpcResponse::BatchResult { results } => {
            if out.json {
                let json_results: Vec<serde_json::Value> = results
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        serde_json::json!({
                            "index": i,
                            "ok": !matches!(r, IpcResponse::Error { .. }),
                            "result": format!("{r:?}"),
                        })
                    })
                    .collect();
                write_json(&json_results)?;
            } else {
                for (i, r) in results.iter().enumerate() {
                    match r {
                        IpcResponse::Error { message } => {
                            eprintln!("op {i}: ERROR: {message}");
                        }
                        _ => {
                            println!("op {i}: ok");
                        }
                    }
                }
            }
            // Return error if any op failed.
            let had_errors = results
                .iter()
                .any(|r| matches!(r, IpcResponse::Error { .. }));
            if had_errors {
                bail!("one or more batch operations failed");
            }
            Ok(())
        }
        other => bail!("unexpected daemon response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_split_basic() {
        let req = parse_line("split 1").unwrap();
        assert!(matches!(
            req,
            IpcRequest::SplitPane {
                pane_id: 1,
                horizontal: false,
                ..
            }
        ));
    }

    #[test]
    fn parse_split_with_flags() {
        let req = parse_line("split 1 --horizontal --ratio 0.6 --spawn 'htop'").unwrap();
        match req {
            IpcRequest::SplitPane {
                pane_id,
                horizontal,
                ratio,
                startup_command,
                ..
            } => {
                assert_eq!(pane_id, 1);
                assert!(horizontal);
                assert_eq!(ratio, Some(0.6));
                assert_eq!(startup_command.as_deref(), Some("htop"));
            }
            _ => panic!("expected SplitPane"),
        }
    }

    #[test]
    fn parse_kill_basic() {
        let req = parse_line("kill 42").unwrap();
        assert!(matches!(req, IpcRequest::KillPane { pane_id: 42 }));
    }

    #[test]
    fn parse_focus_basic() {
        let req = parse_line("focus 3").unwrap();
        assert!(matches!(req, IpcRequest::SelectPane { pane_id: 3 }));
    }

    #[test]
    fn parse_swap_basic() {
        let req = parse_line("swap 1 2").unwrap();
        assert!(matches!(req, IpcRequest::SwapPane { a: 1, b: 2 }));
    }

    #[test]
    fn parse_move_basic() {
        let req = parse_line("move 1 3").unwrap();
        assert!(matches!(
            req,
            IpcRequest::MovePane {
                pane_id: 1,
                target_workspace_id: 3,
            }
        ));
    }

    #[test]
    fn parse_create_workspace_basic() {
        let req = parse_line("create-workspace 1 build").unwrap();
        match req {
            IpcRequest::CreateWorkspace { session_id, name } => {
                assert_eq!(session_id, 1);
                assert_eq!(name.as_deref(), Some("build"));
            }
            _ => panic!("expected CreateWorkspace"),
        }
    }

    #[test]
    fn comment_and_blank_lines_rejected() {
        assert!(parse_line("# comment").is_err());
        assert!(parse_line("").is_err());
    }

    #[test]
    fn shell_tokenize_quotes() {
        let tokens = shell_tokenize("split 1 --spawn 'echo hello world'");
        assert_eq!(tokens, vec!["split", "1", "--spawn", "echo hello world"]);
    }
}
