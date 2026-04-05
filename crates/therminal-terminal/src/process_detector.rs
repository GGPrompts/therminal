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
        if let Some(last) = self.last_scan {
            if last.elapsed() < self.scan_interval {
                return None;
            }
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
            true, // update CPU usage
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

    // Copilot: process name contains "copilot".
    if name.contains("copilot") {
        return Some(AgentType::Copilot);
    }

    None
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
}
