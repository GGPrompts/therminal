//! Process-tree-based agent detection using sysinfo.
//!
//! Walks the process tree below the shell PID to find known AI agents
//! (Claude Code, Codex, Aider, Copilot). This gives definitive answers
//! compared to the heuristic text-matching in [`crate::state_inference`].

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
        }
    }

    /// Override the scan interval (default 3 seconds).
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.scan_interval = interval;
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
}
