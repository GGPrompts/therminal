//! WSL-specific functions: shelling out to `wsl.exe` and parsing its `ps`
//! output to detect agents running inside a WSL distro.
//!
//! These paths are only active when the daemon runs on Windows native and the
//! pane's shell is `wsl.exe` — on Linux/macOS hosts the `wsl.exe` lookup
//! will fail and all functions return empty results gracefully.

use super::DetectedAgent;
use super::classifier::classify_wsl_process;

/// Shell out to `wsl.exe -d <distro> -e ps -eo pid=,ppid=,comm=,args=`
/// and return the raw stdout as a String, or `None` on failure.
/// Used internally by `scan_wsl` and exposed for the daemon's
/// stdout-caching path (tn-ttie).
pub(super) fn fetch_wsl_ps_stdout(distro: &str) -> Option<String> {
    use std::process::Command;
    let output = match Command::new("wsl.exe")
        .args(["-d", distro, "-e", "ps", "-eo", "pid=,ppid=,comm=,args="])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(
                distro,
                error = %e,
                "process_detector: wsl.exe ps failed (probe disabled this tick)"
            );
            return None;
        }
    };
    if !output.status.success() {
        tracing::debug!(
            distro,
            status = ?output.status,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "process_detector: wsl.exe ps non-zero exit"
        );
        return None;
    }
    let cleaned: Vec<u8> = output.stdout.into_iter().filter(|&b| b != 0).collect();
    Some(String::from_utf8_lossy(&cleaned).into_owned())
}

/// Parse the output of `ps -eo pid=,ppid=,comm=,args=` and return any
/// rows that classify as a known agent. Pure function so it can be
/// unit-tested without spawning `wsl.exe`.
///
/// Each row has the layout `<pid> <ppid> <comm> <args...>` with at
/// least one whitespace character between fields. ps may right-align
/// pid/ppid (multiple leading spaces) and comm is a single token, so
/// `split_whitespace().take(3)` gives us the leading three columns and
/// we recover `args` by slicing the rest of the original line.
pub(super) fn parse_wsl_ps(stdout: &str) -> Vec<DetectedAgent> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        // Lock down the first three tokens with their byte ranges so we
        // can compute where `args` begins in the original line.
        let mut tokens = trimmed.split_whitespace();
        let Some(pid_s) = tokens.next() else {
            continue;
        };
        let Some(_ppid_s) = tokens.next() else {
            continue;
        };
        let Some(comm) = tokens.next() else {
            continue;
        };
        // The remaining slice (if any) is the args column. Find it by
        // locating `comm` after the leading two columns and skipping
        // forward past trailing whitespace; this preserves any
        // whitespace inside `args` itself.
        let pid: u32 = match pid_s.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        // Locate the start of `args` by finding `comm` in `trimmed`
        // after `pid_s ppid_s `. Walk past `comm` and any trailing
        // whitespace. Falls back to "" if `comm` was the last token.
        let args = remainder_after_token(trimmed, comm);

        if let Some(agent_type) = classify_wsl_process(comm, args) {
            out.push(DetectedAgent {
                agent_type,
                pid,
                name: comm.to_string(),
            });
        }
    }
    out
}

/// Parse the output of `ps -eo pid=,ppid=,comm=,args=` and return agents
/// that are descendants of `root_pid` (BFS tree-walk). This is the
/// per-pane scoped variant of [`parse_wsl_ps`] used when the WSL-side
/// shell PID is known from OSC 7337 (tn-ttie).
///
/// 1. Parses all rows into a HashMap keyed by pid → (ppid, comm, args).
/// 2. Builds a children map: HashMap<pid, Vec<child_pid>>.
/// 3. BFS-walks from `root_pid` through the children map.
/// 4. Classifies each visited process with `classify_wsl_process`.
///
/// Returns an empty list if `root_pid` is not found in the process table.
// TODO: [code-review] This function is only called within this file — consider making
// it `fn` (private) instead of `pub fn` (88%)
pub(super) fn parse_wsl_ps_tree(stdout: &str, root_pid: u32) -> Vec<DetectedAgent> {
    use std::collections::{HashMap, VecDeque};

    // Step 1: Parse all rows into a flat map.
    let mut procs: HashMap<u32, (u32, String, String)> = HashMap::new();
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let Some(pid_s) = tokens.next() else {
            continue;
        };
        let Some(ppid_s) = tokens.next() else {
            continue;
        };
        let Some(comm) = tokens.next() else {
            continue;
        };
        let pid: u32 = match pid_s.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let ppid: u32 = match ppid_s.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let args = remainder_after_token(trimmed, comm).to_string();
        procs.insert(pid, (ppid, comm.to_string(), args));
    }

    // Step 2: Build a children map.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &(ppid, _, _)) in &procs {
        children.entry(ppid).or_default().push(pid);
    }

    // Step 3: BFS from root_pid.
    let mut agents = Vec::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    // Start with the children of root_pid (not root_pid itself, which
    // is the shell — we want agents running under it).
    if let Some(kids) = children.get(&root_pid) {
        queue.extend(kids);
    }

    // TODO: [code-review] Consider adding a HashSet<u32> visited set to prevent
    // theoretical infinite loops on malformed ps output where pid==ppid (82%)
    while let Some(pid) = queue.pop_front() {
        if let Some((_, comm, args)) = procs.get(&pid) {
            if let Some(agent_type) = classify_wsl_process(comm, args) {
                agents.push(DetectedAgent {
                    agent_type,
                    pid,
                    name: comm.clone(),
                });
            }
            // Continue walking children even if this node is an agent
            // (agents can spawn sub-processes that are also agents).
            if let Some(kids) = children.get(&pid) {
                queue.extend(kids);
            }
        }
    }

    agents
}

/// Find `token` in `line` and return everything after it, with leading
/// whitespace trimmed. Returns `""` if `token` is the tail of the line
/// or doesn't appear at all. Used by [`parse_wsl_ps`] to recover the
/// `args` column without re-tokenising.
fn remainder_after_token<'a>(line: &'a str, token: &str) -> &'a str {
    // Walk forward through `line` looking for an exact run of `token`
    // followed by either end-of-line or a whitespace separator. We
    // can't use `line.find(token)` blindly because the token might
    // appear as a substring earlier (e.g. `comm = "node"` and pid =
    // "12node34"). The realistic ps output never collides like that
    // for the leading three columns, but be defensive.
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(token) {
        let abs = search_from + rel;
        let after = abs + token.len();
        let prev_is_ws = abs == 0 || line.as_bytes()[abs - 1].is_ascii_whitespace();
        let next_is_end_or_ws = after >= line.len() || line.as_bytes()[after].is_ascii_whitespace();
        if prev_is_ws && next_is_end_or_ws {
            return line[after..].trim_start();
        }
        search_from = abs + token.len();
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_inference::AgentType;

    // ── WSL probe parser (tn-966s) ────────────────────────────────────────

    #[test]
    fn wsl_probe_parses_node_claude_as_claude() {
        // Realistic `ps -eo pid=,ppid=,comm=,args=` line.
        let stdout = "  12345  12340 node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\n";
        let agents = parse_wsl_ps(stdout);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Claude);
        assert_eq!(agents[0].pid, 12345);
        assert_eq!(agents[0].name, "node");
    }

    #[test]
    fn wsl_probe_parses_codex_as_codex() {
        let stdout = "  9999  9998 codex codex --resume\n";
        let agents = parse_wsl_ps(stdout);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Codex);
    }

    #[test]
    fn wsl_probe_parses_python_aider_as_aider() {
        let stdout = "  4242  4240 python3 /usr/bin/python3 /home/u/.local/bin/aider\n";
        let agents = parse_wsl_ps(stdout);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Aider);
    }

    #[test]
    fn wsl_probe_skips_non_agents() {
        let stdout = "  1     0 systemd /sbin/init splash\n  100   1 sshd /usr/sbin/sshd -D\n  200 100 bash -bash\n";
        let agents = parse_wsl_ps(stdout);
        assert!(agents.is_empty());
    }

    #[test]
    fn wsl_probe_handles_blank_lines_and_garbage() {
        let stdout = "\n\nnot-a-row\n  X     Y bash bash\n  500   1 codex codex\n";
        let agents = parse_wsl_ps(stdout);
        // Garbage rows are silently dropped; the codex line is picked up.
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Codex);
        assert_eq!(agents[0].pid, 500);
    }

    #[test]
    fn wsl_probe_classifier_matches_native_for_known_agents() {
        // The classifier shape should match `classify_process` for the
        // same agent fingerprint, otherwise the WSL pane and the native
        // pane would disagree on what counts as a Claude run.
        assert_eq!(
            classify_wsl_process("node", "node /opt/claude/cli.js"),
            Some(AgentType::Claude)
        );
        // Native Claude binary (post-2025).
        assert_eq!(
            classify_wsl_process("claude", "claude --resume"),
            Some(AgentType::Claude)
        );
        assert_eq!(
            classify_wsl_process("codex", "codex --resume"),
            Some(AgentType::Codex)
        );
        assert_eq!(
            classify_wsl_process("python3", "python3 /usr/bin/aider --model gpt-4"),
            Some(AgentType::Aider)
        );
        assert_eq!(
            classify_wsl_process("copilot", "copilot --help"),
            Some(AgentType::Copilot)
        );
    }

    #[test]
    fn wsl_probe_classifier_rejects_unrelated() {
        assert_eq!(classify_wsl_process("bash", "bash -i"), None);
        assert_eq!(classify_wsl_process("vim", "vim foo.rs"), None);
    }

    // ── Table-driven WSL ps parsing (tn-cntx) ────────────────────────────

    /// Comprehensive table-driven test for `parse_wsl_ps` covering Claude
    /// Code, Codex, Aider, Copilot process trees, mixed processes, empty
    /// output, malformed lines, and interop processes.
    #[test]
    fn parse_wsl_ps_table_driven() {
        struct Case {
            label: &'static str,
            stdout: &'static str,
            expected: Vec<(AgentType, u32, &'static str)>,
        }
        let cases = [
            // --- Claude Code process tree ---
            Case {
                label: "Claude Code: node with claude in args",
                stdout: "  12345  12340 node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\n",
                expected: vec![(AgentType::Claude, 12345, "node")],
            },
            Case {
                label: "Claude Code: node18 variant",
                stdout: "  555    1 node18 /home/u/.nvm/versions/node/v18/bin/node /usr/local/bin/claude\n",
                expected: vec![(AgentType::Claude, 555, "node18")],
            },
            Case {
                label: "Claude Code: native binary",
                stdout: "  7777  7770 claude claude --resume\n",
                expected: vec![(AgentType::Claude, 7777, "claude")],
            },
            Case {
                label: "Claude Code: native binary with subcommand path",
                stdout: "  8888  1 claude /home/u/.claude/local/claude\n",
                expected: vec![(AgentType::Claude, 8888, "claude")],
            },
            // --- Codex process tree ---
            Case {
                label: "Codex: exact name",
                stdout: "  9999  9998 codex codex --resume\n",
                expected: vec![(AgentType::Codex, 9999, "codex")],
            },
            Case {
                label: "Codex: prefixed name",
                stdout: "  100   1 codex-cli codex-cli run\n",
                expected: vec![(AgentType::Codex, 100, "codex-cli")],
            },
            // --- Aider (Python) ---
            Case {
                label: "Aider: python3 with aider in args",
                stdout: "  4242  4240 python3 /usr/bin/python3 /home/u/.local/bin/aider\n",
                expected: vec![(AgentType::Aider, 4242, "python3")],
            },
            Case {
                label: "Aider: python (no version suffix)",
                stdout: "  300   1 python python -m aider --model gpt-4\n",
                expected: vec![(AgentType::Aider, 300, "python")],
            },
            // --- Copilot CLI ---
            Case {
                label: "Copilot: exact name",
                stdout: "  800   1 copilot copilot --help\n",
                expected: vec![(AgentType::Copilot, 800, "copilot")],
            },
            Case {
                label: "Copilot: prefixed name",
                stdout: "  801   1 copilot-cli copilot-cli suggest\n",
                expected: vec![(AgentType::Copilot, 801, "copilot-cli")],
            },
            // --- Mixed Linux + unrelated processes ---
            Case {
                label: "mixed: agents among non-agents",
                stdout: concat!(
                    "  1     0 systemd /sbin/init splash\n",
                    "  100   1 sshd /usr/sbin/sshd -D\n",
                    "  200 100 bash -bash\n",
                    "  300 200 node /usr/local/bin/claude --json\n",
                    "  400 200 vim foo.rs\n",
                    "  500 200 codex codex plan\n",
                ),
                expected: vec![
                    (AgentType::Claude, 300, "node"),
                    (AgentType::Codex, 500, "codex"),
                ],
            },
            // --- Empty ps output ---
            Case {
                label: "empty output",
                stdout: "",
                expected: vec![],
            },
            Case {
                label: "only whitespace and blank lines",
                stdout: "\n  \n\n  \n",
                expected: vec![],
            },
            // --- Malformed ps lines ---
            Case {
                label: "malformed: no fields at all",
                stdout: "not-a-row\n",
                expected: vec![],
            },
            Case {
                label: "malformed: non-numeric PID",
                stdout: "  X     Y bash bash\n",
                expected: vec![],
            },
            Case {
                label: "malformed: only one field",
                stdout: "  123\n",
                expected: vec![],
            },
            Case {
                label: "malformed: only two fields",
                stdout: "  123  456\n",
                expected: vec![],
            },
            Case {
                label: "malformed mixed with valid",
                stdout: "garbage line\n  X Y bash bash\n  500   1 codex codex\n",
                expected: vec![(AgentType::Codex, 500, "codex")],
            },
            // --- Interop processes (Windows .exe via WSL) ---
            // Note: parse_wsl_ps does NOT filter interop — it runs
            // classify_wsl_process which classifies by comm+args, and
            // interop processes appear with /init as comm. /init does
            // not match any agent pattern, so they are naturally skipped.
            Case {
                label: "interop: /init with Windows exe not classified",
                stdout: "  700   1 init /init /mnt/c/Windows/System32/cmd.exe\n",
                expected: vec![],
            },
            Case {
                label: "interop: Windows node with claude (init comm) not classified",
                stdout: "  701   1 init /init /mnt/c/Program Files/nodejs/node.exe claude\n",
                expected: vec![],
            },
            // --- Right-aligned wide PID columns ---
            Case {
                label: "wide PIDs with extra padding",
                stdout: "    12345678     1234 node /usr/bin/node /opt/claude/cli.js\n",
                expected: vec![(AgentType::Claude, 12345678, "node")],
            },
            // --- Args with spaces preserved ---
            Case {
                label: "args with spaces preserved for matching",
                stdout: "  42   1 python3 /usr/bin/python3 -m aider --model gpt-4 turbo\n",
                expected: vec![(AgentType::Aider, 42, "python3")],
            },
            // --- Server-mode exclusions (tn-mcp-fp) ---
            Case {
                label: "codex mcp-server is not an interactive agent",
                stdout: "  9999  9998 codex codex mcp-server\n",
                expected: vec![],
            },
            Case {
                label: "node claude mcp serve is not an interactive agent",
                stdout: "  555   1 node node /usr/bin/claude serve\n",
                expected: vec![],
            },
            Case {
                label: "codex daemon is not an interactive agent",
                stdout: "  100   1 codex codex daemon\n",
                expected: vec![],
            },
            Case {
                label: "mixed: interactive codex detected, mcp-server skipped",
                stdout: concat!(
                    "  100   1 codex codex --resume\n",
                    "  200   1 codex codex mcp-server\n",
                ),
                expected: vec![(AgentType::Codex, 100, "codex")],
            },
        ];

        for (i, c) in cases.iter().enumerate() {
            let agents = parse_wsl_ps(c.stdout);
            assert_eq!(
                agents.len(),
                c.expected.len(),
                "case {i} ({}) expected {} agents, got {}: {:?}",
                c.label,
                c.expected.len(),
                agents.len(),
                agents
            );
            for (j, (exp_type, exp_pid, exp_name)) in c.expected.iter().enumerate() {
                assert_eq!(
                    agents[j].agent_type, *exp_type,
                    "case {i}.{j} ({}) agent_type mismatch",
                    c.label
                );
                assert_eq!(
                    agents[j].pid, *exp_pid,
                    "case {i}.{j} ({}) pid mismatch",
                    c.label
                );
                assert_eq!(
                    agents[j].name, *exp_name,
                    "case {i}.{j} ({}) name mismatch",
                    c.label
                );
            }
        }
    }

    // ── parse_wsl_ps_tree (tn-ttie) ─────────────────────────────────────

    /// Table-driven tests for `parse_wsl_ps_tree`: per-pane subtree scoping
    /// that BFS-walks from a given root PID.
    #[test]
    fn parse_wsl_ps_tree_table_driven() {
        struct Case {
            label: &'static str,
            stdout: &'static str,
            root_pid: u32,
            expected: Vec<(AgentType, u32, &'static str)>,
        }
        let cases = [
            // Single shell with claude child → finds claude.
            Case {
                label: "single shell with claude child",
                stdout: concat!(
                    "  1     0 systemd /sbin/init\n",
                    "  100   1 bash -bash\n",
                    "  200 100 node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\n",
                ),
                root_pid: 100,
                expected: vec![(AgentType::Claude, 200, "node")],
            },
            // Two shells, only one has claude → only that subtree returns claude.
            Case {
                label: "two shells, only shell B has claude",
                stdout: concat!(
                    "  1     0 systemd /sbin/init\n",
                    "  100   1 bash -bash\n",
                    "  101   1 bash -bash\n",
                    "  200 100 vim foo.rs\n",
                    "  300 101 node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\n",
                ),
                root_pid: 100,
                expected: vec![], // shell 100 has no claude, only vim
            },
            Case {
                label: "two shells, shell B has claude, query B",
                stdout: concat!(
                    "  1     0 systemd /sbin/init\n",
                    "  100   1 bash -bash\n",
                    "  101   1 bash -bash\n",
                    "  200 100 vim foo.rs\n",
                    "  300 101 node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\n",
                ),
                root_pid: 101,
                expected: vec![(AgentType::Claude, 300, "node")],
            },
            // Agent nested 3 levels deep → still found via BFS.
            Case {
                label: "agent nested 3 levels deep",
                stdout: concat!(
                    "  100   1 bash -bash\n",
                    "  200 100 tmux tmux\n",
                    "  300 200 bash bash\n",
                    "  400 300 codex codex --resume\n",
                ),
                root_pid: 100,
                expected: vec![(AgentType::Codex, 400, "codex")],
            },
            // Empty tree → returns empty.
            Case {
                label: "empty stdout",
                stdout: "",
                root_pid: 100,
                expected: vec![],
            },
            // Nonexistent root PID → returns empty.
            Case {
                label: "nonexistent root PID",
                stdout: concat!(
                    "  100   1 bash -bash\n",
                    "  200 100 node /usr/lib/claude-code/cli.js\n",
                ),
                root_pid: 999,
                expected: vec![],
            },
            // Multiple agents in subtree → finds all of them.
            Case {
                label: "multiple agents in subtree",
                stdout: concat!(
                    "  100   1 bash -bash\n",
                    "  200 100 node /usr/lib/claude-code/cli.js\n",
                    "  300 100 codex codex --resume\n",
                ),
                root_pid: 100,
                expected: vec![
                    (AgentType::Claude, 200, "node"),
                    (AgentType::Codex, 300, "codex"),
                ],
            },
            // Root PID is the agent itself → agent is not returned
            // (we start from children of root, not root itself).
            Case {
                label: "root PID is the agent itself",
                stdout: concat!(
                    "  100   1 bash -bash\n",
                    "  200 100 node /usr/lib/claude-code/cli.js\n",
                ),
                root_pid: 200,
                expected: vec![],
            },
            // Server-mode process excluded even in subtree.
            Case {
                label: "server-mode claude skipped in subtree",
                stdout: concat!(
                    "  100   1 bash -bash\n",
                    "  200 100 node node /usr/bin/claude serve\n",
                ),
                root_pid: 100,
                expected: vec![],
            },
        ];

        for (i, c) in cases.iter().enumerate() {
            let mut agents = parse_wsl_ps_tree(c.stdout, c.root_pid);
            // Sort by PID for stable comparison (BFS order depends on
            // HashMap iteration order which is non-deterministic).
            agents.sort_by_key(|a| a.pid);
            let mut expected: Vec<_> = c.expected.clone();
            expected.sort_by_key(|&(_, pid, _)| pid);

            assert_eq!(
                agents.len(),
                expected.len(),
                "case {i} ({}) expected {} agents, got {}: {:?}",
                c.label,
                expected.len(),
                agents.len(),
                agents
            );
            for (j, (exp_type, exp_pid, exp_name)) in expected.iter().enumerate() {
                assert_eq!(
                    agents[j].agent_type, *exp_type,
                    "case {i}.{j} ({}) agent_type mismatch",
                    c.label
                );
                assert_eq!(
                    agents[j].pid, *exp_pid,
                    "case {i}.{j} ({}) pid mismatch",
                    c.label
                );
                assert_eq!(
                    agents[j].name, *exp_name,
                    "case {i}.{j} ({}) name mismatch",
                    c.label
                );
            }
        }
    }

    // ── CRLF and whitespace edge cases ─────────────────────────────────

    #[test]
    fn parse_wsl_ps_crlf_line_endings() {
        // wsl.exe output may have CRLF line endings when piped through
        // Windows-side tools. `str::lines()` handles both \n and \r\n.
        let stdout = "  12345  12340 node /usr/lib/claude-code/cli.js\r\n  9999  9998 codex codex --resume\r\n";
        let agents = parse_wsl_ps(stdout);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].agent_type, AgentType::Claude);
        assert_eq!(agents[1].agent_type, AgentType::Codex);
    }

    #[test]
    fn parse_wsl_ps_tabs_in_output() {
        // Some ps implementations use tabs instead of spaces.
        let stdout = "12345\t12340\tnode\t/usr/lib/claude-code/cli.js\n";
        let agents = parse_wsl_ps(stdout);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Claude);
    }

    #[test]
    fn parse_wsl_ps_tree_nonexistent_root_returns_empty() {
        let stdout = concat!(
            "  100   1 bash -bash\n",
            "  200 100 node /usr/lib/claude-code/cli.js\n",
        );
        let agents = parse_wsl_ps_tree(stdout, 99999);
        assert!(agents.is_empty());
    }

    #[test]
    fn parse_wsl_ps_tree_root_with_no_children() {
        // Root exists but has no children — returns empty.
        let stdout = "  100   1 bash -bash\n";
        let agents = parse_wsl_ps_tree(stdout, 100);
        assert!(agents.is_empty());
    }

    #[test]
    fn parse_wsl_ps_tree_deeply_nested_agent() {
        // Agent is 4 levels deep: bash → tmux → bash → screen → codex
        let stdout = concat!(
            "  100   1 bash -bash\n",
            "  200 100 tmux tmux\n",
            "  300 200 bash bash\n",
            "  400 300 screen screen\n",
            "  500 400 codex codex --resume\n",
        );
        let agents = parse_wsl_ps_tree(stdout, 100);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Codex);
        assert_eq!(agents[0].pid, 500);
    }

    #[test]
    fn parse_wsl_ps_tree_crlf_line_endings() {
        let stdout = "  100   1 bash -bash\r\n  200 100 node /usr/lib/claude-code/cli.js\r\n";
        let agents = parse_wsl_ps_tree(stdout, 100);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Claude);
    }

    // ── remainder_after_token edge cases ────────────────────────────────

    #[test]
    fn remainder_after_token_basic() {
        assert_eq!(remainder_after_token("100 200 bash -bash", "bash"), "-bash");
    }

    #[test]
    fn remainder_after_token_no_remainder() {
        assert_eq!(remainder_after_token("100 200 bash", "bash"), "");
    }

    #[test]
    fn remainder_after_token_not_found() {
        assert_eq!(remainder_after_token("100 200 bash", "node"), "");
    }

    #[test]
    fn remainder_after_token_comm_appears_in_pid() {
        // Token "node" also appears as substring in pid "12node34" — must
        // find the standalone token, not the substring.
        let line = "12345 100 node /usr/lib/claude/cli.js";
        assert_eq!(
            remainder_after_token(line, "node"),
            "/usr/lib/claude/cli.js"
        );
    }

    #[test]
    fn remainder_after_token_preserves_internal_spaces() {
        let line = "100 200 python3 python3 -m aider --model gpt-4 turbo";
        assert_eq!(
            remainder_after_token(line, "python3"),
            "python3 -m aider --model gpt-4 turbo"
        );
    }

    /// Regression: global `parse_wsl_ps` still works unchanged after the
    /// tree-walking addition (tn-ttie).
    #[test]
    fn parse_wsl_ps_global_still_works_after_tree_walk_addition() {
        let stdout = concat!(
            "  1     0 systemd /sbin/init\n",
            "  100   1 bash -bash\n",
            "  200 100 node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\n",
            "  300 100 codex codex --resume\n",
        );
        let agents = parse_wsl_ps(stdout);
        assert_eq!(agents.len(), 2);
        // Global scan finds ALL agents regardless of tree structure.
        let types: Vec<_> = agents.iter().map(|a| a.agent_type).collect();
        assert!(types.contains(&AgentType::Claude));
        assert!(types.contains(&AgentType::Codex));
    }
}
