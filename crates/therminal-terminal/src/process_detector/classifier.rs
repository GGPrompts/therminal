//! Process classification logic for both sysinfo and WSL probe paths.
//!
//! Both `classify_process` (sysinfo) and `classify_wsl_process` (WSL ps
//! text) are kept in sync — every agent type recognised by one path must
//! also be recognised by the other.

use sysinfo::Process;

use crate::state_inference::AgentType;

/// Classify a single process as an agent type based on its name and command line.
pub(super) fn classify_process(process: &Process) -> Option<AgentType> {
    let name = process.name().to_string_lossy().to_lowercase();
    let cmd_parts = process.cmd();
    let cmd_lower: Vec<String> = cmd_parts
        .iter()
        .map(|s| s.to_string_lossy().to_lowercase())
        .collect();
    let cmd_joined = cmd_lower.join(" ");

    // WSL2 guard: Windows processes launched via WSL interop appear as Linux PIDs
    // with their exe rooted at `/init` and their cmdline starting with "/init /mnt/c/...".
    // Skip these to avoid false positives from Windows-side node.exe/python.exe
    // instances that happen to have "claude" or "aider" in their command line.
    if is_wsl2_interop_process(&cmd_lower) {
        return None;
    }

    // Guard: skip processes running in server/daemon mode — these are MCP
    // servers or background services, not interactive agent sessions.
    if is_server_mode(&cmd_joined) {
        return None;
    }

    // Claude Code: Node.js process with "claude" in the command line,
    // OR native binary named "claude" / "claude-*" (post-2025 Rust build).
    if name.contains("node") && cmd_joined.contains("claude") {
        return Some(AgentType::Claude);
    }
    if name == "claude" || name.starts_with("claude-") {
        return Some(AgentType::Claude);
    }

    // Codex: process named "codex" (or containing it).
    if name == "codex" || name.starts_with("codex-") {
        return Some(AgentType::Codex);
    }

    // Aider: Python process with "aider" in the command line.
    if name.contains("python") && cmd_joined.contains("aider") {
        return Some(AgentType::Aider);
    }

    // Copilot: process name is exactly "copilot" or a known copilot- prefix.
    // Audited for tn-3pkv: previously `name.contains("copilot")` which would
    // false-positive on any binary whose name happened to contain the substring.
    // TFE (the user-reported false positive) does not match either form, so
    // the actual root cause for tn-3pkv lives in the output-pattern matcher,
    // but we tighten this defensively while we're here.
    if name == "copilot" || name.starts_with("copilot-") {
        return Some(AgentType::Copilot);
    }

    None
}

/// Classify a single process from `wsl.exe ps` output. Mirrors
/// [`classify_process`] but works on the (comm, args) string pair instead
/// of a sysinfo `Process`. The two paths are kept in sync — every agent
/// type recognised by sysinfo must also be recognised here.
pub(super) fn classify_wsl_process(comm: &str, args: &str) -> Option<AgentType> {
    let comm_lower = comm.to_lowercase();
    let args_lower = args.to_lowercase();

    // Guard: skip processes running in server/daemon mode — these are MCP
    // servers or background services, not interactive agent sessions.
    if is_server_mode(&args_lower) {
        return None;
    }

    // Claude Code: Node.js process with "claude" in the command line,
    // OR native binary named "claude" / "claude-*" (post-2025 Rust build).
    if comm_lower.contains("node") && args_lower.contains("claude") {
        return Some(AgentType::Claude);
    }
    if comm_lower == "claude" || comm_lower.starts_with("claude-") {
        return Some(AgentType::Claude);
    }
    // Codex: process named exactly "codex" or "codex-…".
    if comm_lower == "codex" || comm_lower.starts_with("codex-") {
        return Some(AgentType::Codex);
    }
    // Aider: Python process with "aider" in the command line.
    if comm_lower.contains("python") && args_lower.contains("aider") {
        return Some(AgentType::Aider);
    }
    // Copilot CLI.
    if comm_lower == "copilot" || comm_lower.starts_with("copilot-") {
        return Some(AgentType::Copilot);
    }
    None
}

/// Return `true` if the command line indicates a server/daemon mode rather
/// than an interactive agent session. Matches subcommands like `mcp-server`,
/// `mcp serve`, `serve`, `daemon` that agents use for background services.
pub(super) fn is_server_mode(args_lower: &str) -> bool {
    // Check for common server-mode subcommands. We look for whole tokens
    // to avoid false positives on e.g. "serverstatus" or "mcp-server-config".
    let server_tokens = ["mcp-server", "mcp server", "serve", "daemon"];
    for token in &server_tokens {
        // Token must appear as a standalone argument: preceded by whitespace
        // (or start of string) and followed by whitespace/end-of-string.
        if let Some(pos) = args_lower.find(token) {
            let before_ok = pos == 0 || args_lower.as_bytes()[pos - 1] == b' ';
            let after = pos + token.len();
            let after_ok = after >= args_lower.len() || args_lower.as_bytes()[after] == b' ';
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// On WSL2, Windows `.exe` files run as Linux processes with `/init` as the
/// actual ELF interpreter. The full command line looks like:
///
///   `/init /mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe ...`
///
/// We detect this by checking whether the first argument is `/init` and the
/// second starts with `/mnt/`.
pub(super) fn is_wsl2_interop_process(cmd_lower: &[String]) -> bool {
    if cmd_lower.len() < 2 {
        return false;
    }
    // cmd_lower[0] is the interpreter path on WSL2 interop processes.
    // Use the lowercase version; /init is already lowercase.
    cmd_lower[0].ends_with("/init") && cmd_lower[1].starts_with("/mnt/")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── WSL2 interop guard ────────────────────────────────────────────────

    #[test]
    fn wsl2_interop_process_detected() {
        // Windows powershell.exe launched via WSL2 binfmt_misc interop:
        // cmdline = ["/init", "/mnt/c/Windows/System32/.../powershell.exe", "-Command", "..."]
        let cmd = vec![
            "/init".to_string(),
            "/mnt/c/windows/system32/windowspowershell/v1.0/powershell.exe".to_string(),
            "-command".to_string(),
            "some-script".to_string(),
        ];
        assert!(is_wsl2_interop_process(&cmd));
    }

    #[test]
    fn wsl2_interop_windows_node_with_claude_not_false_positive() {
        // A Windows node.exe with "claude" in the args should be filtered out.
        let cmd = vec![
            "/init".to_string(),
            "/mnt/c/program files/nodejs/node.exe".to_string(),
            "/mnt/c/users/alice/appdata/roaming/npm/node_modules/.bin/claude".to_string(),
        ];
        assert!(is_wsl2_interop_process(&cmd));
    }

    #[test]
    fn linux_node_not_flagged_as_interop() {
        // A native Linux node process should NOT be filtered.
        let cmd = vec![
            "/home/alice/.nvm/versions/node/v20/bin/node".to_string(),
            "/usr/local/lib/node_modules/.bin/claude".to_string(),
        ];
        assert!(!is_wsl2_interop_process(&cmd));
    }

    #[test]
    fn wsl2_interop_requires_mnt_prefix_on_second_arg() {
        // /init without /mnt/ second arg is not WSL interop.
        let cmd = vec!["/init".to_string(), "/usr/bin/bash".to_string()];
        assert!(!is_wsl2_interop_process(&cmd));
    }

    #[test]
    fn wsl2_interop_empty_cmd_not_flagged() {
        assert!(!is_wsl2_interop_process(&[]));
        assert!(!is_wsl2_interop_process(&["/init".to_string()]));
    }

    // ── Server-mode guard ─────────────────────────────────────────────────

    #[test]
    fn is_server_mode_catches_known_subcommands() {
        assert!(is_server_mode("codex mcp-server"));
        assert!(is_server_mode("codex mcp server"));
        assert!(is_server_mode("node /usr/bin/claude serve"));
        assert!(is_server_mode("codex daemon"));
    }

    #[test]
    fn is_server_mode_allows_interactive_sessions() {
        assert!(!is_server_mode("codex --resume"));
        assert!(!is_server_mode("codex plan"));
        assert!(!is_server_mode("node /usr/bin/claude --json"));
        assert!(!is_server_mode(
            "codex --config /home/user/server/config.toml"
        ));
    }

    // ── classify_wsl_process table-driven ─────────────────────────────────

    /// Table-driven test for `classify_wsl_process` ensuring parity with
    /// the sysinfo-based `classify_process` for all known agent types.
    #[test]
    fn classify_wsl_process_table_driven() {
        struct Case {
            label: &'static str,
            comm: &'static str,
            args: &'static str,
            expected: Option<AgentType>,
        }
        let cases = [
            // Claude variants
            Case {
                label: "Claude: node + claude in args",
                comm: "node",
                args: "node /usr/lib/claude-code/cli.js",
                expected: Some(AgentType::Claude),
            },
            Case {
                label: "Claude: nodejs + claude in args",
                comm: "nodejs",
                args: "nodejs /opt/claude",
                expected: Some(AgentType::Claude),
            },
            Case {
                label: "Claude: node without claude => None",
                comm: "node",
                args: "node /usr/lib/something-else.js",
                expected: None,
            },
            Case {
                label: "Claude: native binary (exact)",
                comm: "claude",
                args: "claude --resume",
                expected: Some(AgentType::Claude),
            },
            Case {
                label: "Claude: native binary (prefixed)",
                comm: "claude-desktop",
                args: "claude-desktop",
                expected: Some(AgentType::Claude),
            },
            Case {
                label: "Claude: native binary case-insensitive",
                comm: "CLAUDE",
                args: "CLAUDE",
                expected: Some(AgentType::Claude),
            },
            // Codex variants
            Case {
                label: "Codex: exact match",
                comm: "codex",
                args: "codex --resume",
                expected: Some(AgentType::Codex),
            },
            Case {
                label: "Codex: prefixed",
                comm: "codex-agent",
                args: "codex-agent run",
                expected: Some(AgentType::Codex),
            },
            Case {
                label: "Codex: substring not matched (mycodex)",
                comm: "mycodex",
                args: "mycodex run",
                expected: None,
            },
            // Aider variants
            Case {
                label: "Aider: python3 + aider",
                comm: "python3",
                args: "python3 /usr/bin/aider",
                expected: Some(AgentType::Aider),
            },
            Case {
                label: "Aider: python + aider",
                comm: "python",
                args: "python -m aider",
                expected: Some(AgentType::Aider),
            },
            Case {
                label: "Aider: python without aider => None",
                comm: "python3",
                args: "python3 /usr/bin/flask run",
                expected: None,
            },
            // Copilot variants
            Case {
                label: "Copilot: exact match",
                comm: "copilot",
                args: "copilot suggest",
                expected: Some(AgentType::Copilot),
            },
            Case {
                label: "Copilot: prefixed",
                comm: "copilot-cli",
                args: "copilot-cli suggest",
                expected: Some(AgentType::Copilot),
            },
            // Non-agent processes
            Case {
                label: "bash is not an agent",
                comm: "bash",
                args: "bash -i",
                expected: None,
            },
            Case {
                label: "vim is not an agent",
                comm: "vim",
                args: "vim foo.rs",
                expected: None,
            },
            Case {
                label: "empty comm",
                comm: "",
                args: "",
                expected: None,
            },
            // Case sensitivity
            Case {
                label: "Claude: case-insensitive NODE + CLAUDE",
                comm: "NODE",
                args: "NODE /usr/lib/CLAUDE/cli.js",
                expected: Some(AgentType::Claude),
            },
            Case {
                label: "Codex: case-insensitive CODEX",
                comm: "CODEX",
                args: "CODEX run",
                expected: Some(AgentType::Codex),
            },
            // Server-mode exclusions (tn-mcp-fp)
            Case {
                label: "Codex mcp-server is not an agent",
                comm: "codex",
                args: "codex mcp-server",
                expected: None,
            },
            Case {
                label: "Claude mcp serve is not an agent (node)",
                comm: "node",
                args: "node /usr/bin/claude serve",
                expected: None,
            },
            Case {
                label: "Claude native binary in serve mode is not an agent",
                comm: "claude",
                args: "claude mcp serve",
                expected: None,
            },
            Case {
                label: "Codex daemon mode is not an agent",
                comm: "codex",
                args: "codex daemon",
                expected: None,
            },
            Case {
                label: "Copilot serve is not an agent",
                comm: "copilot",
                args: "copilot serve",
                expected: None,
            },
            Case {
                label: "serve substring in path does not trigger exclusion",
                comm: "codex",
                args: "codex --config /home/user/server/config.toml",
                expected: Some(AgentType::Codex),
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            let result = classify_wsl_process(c.comm, c.args);
            assert_eq!(
                result, c.expected,
                "case {i} ({}): comm={:?} args={:?}",
                c.label, c.comm, c.args
            );
        }
    }
}
