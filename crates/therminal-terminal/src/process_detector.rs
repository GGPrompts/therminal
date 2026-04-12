//! Process-tree-based agent detection using sysinfo.
//!
//! Walks the process tree below the shell PID to find known AI agents
//! (Claude Code, Codex, Aider, Copilot). This gives definitive answers
//! compared to the heuristic text-matching in [`crate::state_inference`].
//!
//! ## WSL probe mode (tn-966s)
//!
//! When `therminal-daemon` runs as a Windows native process and the pane
//! is a WSL shell, the sysinfo path is blind: the host process tree only
//! sees `wsl.exe` and the Linux child processes are invisible across the
//! WSL boundary. The detector supports an alternate **WSL probe** mode
//! activated via [`ProcessDetector::with_wsl_distro`]. In this mode the
//! `scan()` method shells out to `wsl.exe -d <distro> -e ps -eo
//! pid,ppid,comm,args` and parses the output to find Claude / Codex /
//! Aider / Copilot inside the distro. The shell-out is much slower than
//! sysinfo (typically 50–200 ms per scan vs <1 ms) so this path is only
//! taken when explicitly enabled by the daemon's per-pane configuration.

use std::time::{Duration, Instant};

use sysinfo::{Pid, Process, System};

use crate::state_inference::AgentType;

/// Default interval between process tree scans.
const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(3);

/// An agent detected via process tree inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedAgent {
    /// The type of agent.
    pub agent_type: AgentType,
    /// The OS process ID.
    pub pid: u32,
    /// The process name (executable basename).
    pub name: String,
}

/// Periodically scans the process tree below a shell PID to detect running agents.
pub struct ProcessDetector {
    system: System,
    shell_pid: Option<u32>,
    scan_interval: Duration,
    last_scan: Option<Instant>,
    /// When `Some(distro)`, `scan()` shells out to `wsl.exe -d <distro>
    /// -e ps -eo pid,ppid,comm,args` and ignores `shell_pid`. Used when
    /// the daemon runs on Windows native and the pane is a WSL shell —
    /// the host sysinfo walker cannot see across the WSL boundary
    /// (tn-966s). `None` keeps the existing native scan path.
    wsl_distro: Option<String>,
}

impl ProcessDetector {
    /// Create a new detector. `shell_pid` is the PID of the shell process
    /// spawned by portable-pty; all children of this PID will be inspected.
    pub fn new(shell_pid: Option<u32>) -> Self {
        Self {
            system: System::new(),
            shell_pid,
            scan_interval: DEFAULT_SCAN_INTERVAL,
            last_scan: None,
            wsl_distro: None,
        }
    }

    /// Override the scan interval (default 3 seconds).
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.scan_interval = interval;
        self
    }

    /// Switch this detector into **WSL probe mode**. Subsequent scans
    /// shell out to `wsl.exe -d <distro> -e ps` instead of walking the
    /// host process tree. Used by the daemon on Windows native when
    /// the pane is a WSL shell. Setting an empty string is a no-op.
    pub fn with_wsl_distro(mut self, distro: impl Into<String>) -> Self {
        let s = distro.into();
        if !s.is_empty() {
            self.wsl_distro = Some(s);
        }
        self
    }

    /// Update the shell PID (e.g., after a PTY respawn).
    pub fn set_shell_pid(&mut self, pid: u32) {
        self.shell_pid = Some(pid);
    }

    /// Perform a scan only if the interval has elapsed since the last scan.
    /// Returns `None` if it is not yet time, or `Some(agents)` with the
    /// (possibly empty) list of detected agents.
    pub fn scan_if_due(&mut self) -> Option<Vec<DetectedAgent>> {
        if let Some(last) = self.last_scan
            && last.elapsed() < self.scan_interval
        {
            return None;
        }
        Some(self.scan())
    }

    /// Force an immediate scan regardless of the interval timer.
    pub fn scan(&mut self) -> Vec<DetectedAgent> {
        self.last_scan = Some(Instant::now());

        if let Some(distro) = self.wsl_distro.clone() {
            return self.scan_wsl(&distro);
        }

        let shell_pid = match self.shell_pid {
            Some(pid) => pid,
            None => return Vec::new(),
        };

        self.system.refresh_processes(
            sysinfo::ProcessesToUpdate::All,
            false, // skip CPU usage — we only need the process tree
        );

        let mut agents = Vec::new();
        let mut stack: Vec<Pid> = vec![Pid::from_u32(shell_pid)];

        // BFS walk of the process tree.
        while let Some(current) = stack.pop() {
            for (pid, process) in self.system.processes() {
                if process.parent() == Some(current) {
                    if let Some(agent) = classify_process(process) {
                        agents.push(DetectedAgent {
                            agent_type: agent,
                            pid: pid.as_u32(),
                            name: process.name().to_string_lossy().into_owned(),
                        });
                    }
                    stack.push(*pid);
                }
            }
        }

        agents
    }

    /// Shell out to `wsl.exe -d <distro> -e ps -eo pid=,ppid=,comm=,args=`
    /// and classify the resulting Linux processes. Returns the list of
    /// detected agents inside the WSL distro.
    ///
    /// Slower than the sysinfo path (typically 50–200 ms) so the daemon's
    /// `process_detector_task` keeps the standard 3 s scan interval. The
    /// command is invoked the same way on every platform that supports it
    /// — on non-Windows builds the `wsl.exe` lookup will fail and the
    /// function returns an empty list.
    fn scan_wsl(&mut self, distro: &str) -> Vec<DetectedAgent> {
        use std::process::Command;
        // -eo with `=` suffix suppresses headers and produces a stable
        // column layout: <pid> <ppid> <comm> <args...>. The args column
        // can contain spaces — we split off the first three fields and
        // treat the rest as the command line.
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
                return Vec::new();
            }
        };
        if !output.status.success() {
            tracing::debug!(
                distro,
                status = ?output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "process_detector: wsl.exe ps non-zero exit"
            );
            return Vec::new();
        }
        // wsl.exe writes UTF-8 on -e mode (no UTF-16 BOM); but be defensive
        // and strip embedded NULs in case a custom shell wrapper inserts them.
        let cleaned: Vec<u8> = output.stdout.into_iter().filter(|&b| b != 0).collect();
        let stdout = String::from_utf8_lossy(&cleaned);
        parse_wsl_ps(&stdout)
    }
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
fn parse_wsl_ps(stdout: &str) -> Vec<DetectedAgent> {
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

/// Classify a single process from `wsl.exe ps` output. Mirrors
/// [`classify_process`] but works on the (comm, args) string pair instead
/// of a sysinfo `Process`. The two paths are kept in sync — every agent
/// type recognised by sysinfo must also be recognised here.
fn classify_wsl_process(comm: &str, args: &str) -> Option<AgentType> {
    let comm_lower = comm.to_lowercase();
    let args_lower = args.to_lowercase();

    // Guard: skip processes running in server/daemon mode — these are MCP
    // servers or background services, not interactive agent sessions.
    if is_server_mode(&args_lower) {
        return None;
    }

    // Claude Code: Node.js process with "claude" in the command line.
    if comm_lower.contains("node") && args_lower.contains("claude") {
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

/// Classify a single process as an agent type based on its name and command line.
fn classify_process(process: &Process) -> Option<AgentType> {
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

    // Claude Code: Node.js process with "claude" in the command line.
    if name.contains("node") && cmd_joined.contains("claude") {
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

/// Return `true` if this process is a Windows executable launched via WSL2
/// binfmt_misc interop (i.e., the interpreter is `/init` and the binary path
/// starts with `/mnt/`).
///
/// Return `true` if the command line indicates a server/daemon mode rather
/// than an interactive agent session. Matches subcommands like `mcp-server`,
/// `mcp serve`, `serve`, `daemon` that agents use for background services.
fn is_server_mode(args_lower: &str) -> bool {
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
fn is_wsl2_interop_process(cmd_lower: &[String]) -> bool {
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

    #[test]
    fn scan_if_due_respects_interval() {
        let mut detector = ProcessDetector::new(None).with_interval(Duration::from_secs(60));

        // First scan should always fire.
        let result = detector.scan_if_due();
        assert!(result.is_some());

        // Immediate second scan should be suppressed.
        let result = detector.scan_if_due();
        assert!(result.is_none());
    }

    #[test]
    fn scan_if_due_fires_after_interval() {
        let mut detector = ProcessDetector::new(None).with_interval(Duration::from_millis(0));

        let result = detector.scan_if_due();
        assert!(result.is_some());

        // With zero interval, next scan should also fire.
        let result = detector.scan_if_due();
        assert!(result.is_some());
    }

    #[test]
    fn scan_returns_empty_without_shell_pid() {
        let mut detector = ProcessDetector::new(None);
        let agents = detector.scan();
        assert!(agents.is_empty());
    }

    #[test]
    fn scan_returns_empty_for_nonexistent_pid() {
        // PID 999999999 almost certainly does not exist.
        let mut detector = ProcessDetector::new(Some(999_999_999));
        let agents = detector.scan();
        assert!(agents.is_empty());
    }

    #[test]
    fn detected_agent_equality() {
        let a = DetectedAgent {
            agent_type: AgentType::Claude,
            pid: 1234,
            name: "node".into(),
        };
        let b = DetectedAgent {
            agent_type: AgentType::Claude,
            pid: 1234,
            name: "node".into(),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn set_shell_pid_updates() {
        let mut detector = ProcessDetector::new(None);
        assert!(detector.shell_pid.is_none());
        detector.set_shell_pid(42);
        assert_eq!(detector.shell_pid, Some(42));
    }

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

    #[test]
    fn detector_with_wsl_distro_takes_probe_path() {
        // Smoke: with a distro set the scan path is the WSL probe.
        // Without a real `wsl.exe` available the probe returns empty,
        // which is fine for this test — we just want to verify the
        // dispatch doesn't panic.
        let mut detector = ProcessDetector::new(None).with_wsl_distro("Ubuntu");
        let agents = detector.scan();
        // No assertions on contents — `wsl.exe` is unlikely to exist on
        // the CI Linux runner. The important thing is the call returns
        // without panic.
        let _ = agents;
    }

    #[test]
    fn detector_with_empty_wsl_distro_is_noop_setter() {
        let detector = ProcessDetector::new(Some(42)).with_wsl_distro("");
        assert!(detector.wsl_distro.is_none());
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
                label: "Claude mcp serve is not an agent",
                comm: "node",
                args: "node /usr/bin/claude serve",
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
