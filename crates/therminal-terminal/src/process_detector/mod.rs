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

mod classifier;
mod wsl_probe;

use std::time::{Duration, Instant};

use sysinfo::{Pid, System};
use therminal_protocol::daemon::PaneIdentity;

use crate::state_inference::AgentType;

use classifier::classify_process;
use wsl_probe::{parse_wsl_identity_tree, parse_wsl_ps_tree};

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

/// One safely scoped process-tree observation for a pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessObservation {
    pub agents: Vec<DetectedAgent>,
    pub identity: PaneIdentity,
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
    /// WSL-side shell PID captured via OSC 7337 (tn-ttie). When set,
    /// `scan_wsl` BFS-walks from this PID instead of scanning the entire
    /// distro. `None` falls back to the global `parse_wsl_ps` path.
    wsl_shell_pid: Option<u32>,
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
            wsl_shell_pid: None,
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

    /// Set the WSL-side shell PID for per-pane subtree scoping (tn-ttie).
    /// When set, `scan_wsl` will BFS-walk from this PID instead of
    /// scanning the entire distro.
    pub fn set_wsl_shell_pid(&mut self, pid: u32) {
        self.wsl_shell_pid = Some(pid);
    }

    /// Perform a scan only if the interval has elapsed since the last scan.
    /// Returns `None` if it is not yet time, or `Some(agents)` with the
    /// (possibly empty) list of detected agents.
    pub fn scan_if_due(&mut self) -> Option<Vec<DetectedAgent>> {
        self.scan_observation_if_due().map(|scan| scan.agents)
    }

    /// Perform a combined identity/agent scan when the interval elapses.
    pub fn scan_observation_if_due(&mut self) -> Option<ProcessObservation> {
        if let Some(last) = self.last_scan
            && last.elapsed() < self.scan_interval
        {
            return None;
        }
        Some(self.scan_observation())
    }

    /// Force an immediate scan regardless of the interval timer.
    pub fn scan(&mut self) -> Vec<DetectedAgent> {
        self.scan_observation().agents
    }

    /// Force an immediate scan and return both agent detection and the
    /// foreground program identity from the same process-tree snapshot.
    pub fn scan_observation(&mut self) -> ProcessObservation {
        self.last_scan = Some(Instant::now());

        if let Some(distro) = self.wsl_distro.clone() {
            return self.scan_wsl_observation(&distro).unwrap_or_default();
        }

        let shell_pid = match self.shell_pid {
            Some(pid) => pid,
            None => return ProcessObservation::default(),
        };

        self.system.refresh_processes(
            sysinfo::ProcessesToUpdate::All,
            true, // remove dead descendants so exited applications clear promptly
        );

        let mut agents = Vec::new();
        let mut current: Option<(usize, u32, String, bool)> = None;
        let mut shell: Option<(usize, String)> = None;
        let mut stack: Vec<(Pid, usize)> = vec![(Pid::from_u32(shell_pid), 0)];

        // BFS walk of the process tree.
        while let Some((current_pid, depth)) = stack.pop() {
            for (pid, process) in self.system.processes() {
                if process.parent() == Some(current_pid) {
                    let child_depth = depth + 1;
                    let name = process.name().to_string_lossy().into_owned();
                    if let Some(agent) = classify_process(process) {
                        agents.push(DetectedAgent {
                            agent_type: agent,
                            pid: pid.as_u32(),
                            name: name.clone(),
                        });
                    }
                    if let Some(label) = shell_label(&name) {
                        if shell.as_ref().is_none_or(|(best, _)| child_depth >= *best) {
                            shell = Some((child_depth, label));
                        }
                    } else if current
                        .as_ref()
                        .is_none_or(|(best, _, _, _)| child_depth >= *best)
                    {
                        current = Some((
                            child_depth,
                            pid.as_u32(),
                            display_application(&name, classify_process(process)),
                            is_wsl_program(&name),
                        ));
                    }
                    stack.push((*pid, child_depth));
                }
            }
        }

        let mut identity = PaneIdentity {
            shell: shell.map(|(_, value)| value),
            application: current.as_ref().map(|(_, _, name, _)| name.clone()),
            ..Default::default()
        };
        // An interactive agent is the pane's application even when it has a
        // deeper transient child (`rg`, compiler, shell tool). Agent status
        // still comes exclusively from AgentRegistry/integrations.
        if let Some(agent) = agents.first() {
            identity.application = Some(display_application(&agent.name, Some(agent.agent_type)));
        }
        if current.as_ref().is_some_and(|(_, _, _, wsl)| *wsl) {
            // We can see the Windows WSL boundary, but without a Linux root PID
            // it is unsafe to attribute distro-global processes to this pane.
            identity.environment = Some("WSL".to_string());
            identity.application = None;
            identity.shell = None;
        }
        ProcessObservation { agents, identity }
    }

    /// Classify WSL processes from pre-fetched `ps` stdout (tn-ttie).
    ///
    /// The daemon's `tick_once` caches the raw stdout across panes in
    /// the same distro to avoid redundant `wsl.exe` subprocess calls.
    /// Each pane then calls this method with the shared stdout and gets
    /// results scoped to its own subtree (or global if no root PID is
    /// set).
    pub fn classify_wsl_stdout(&self, stdout: &str) -> Vec<DetectedAgent> {
        if let Some(root_pid) = self.wsl_shell_pid {
            parse_wsl_ps_tree(stdout, root_pid)
        } else {
            // A distro-wide process list cannot be safely attributed to one
            // pane. Wait for OSC 7337 to supply the pane's Linux root PID.
            Vec::new()
        }
    }

    /// Classify a pre-fetched WSL process table into a pane-scoped identity.
    /// Returns `None` until OSC 7337 supplies the Linux shell root PID.
    pub fn observe_wsl_stdout(&self, stdout: &str, distro: &str) -> Option<ProcessObservation> {
        let root_pid = self.wsl_shell_pid?;
        Some(ProcessObservation {
            agents: parse_wsl_ps_tree(stdout, root_pid),
            identity: parse_wsl_identity_tree(stdout, root_pid, distro),
        })
    }

    /// Shell out to `wsl.exe -d <distro> -e ps -eo pid=,ppid=,comm=,args=`
    /// and return the raw stdout as a String, or `None` on failure.
    /// Used internally by `scan_wsl` and exposed for the daemon's
    /// stdout-caching path (tn-ttie).
    pub fn fetch_wsl_ps_stdout(distro: &str) -> Option<String> {
        wsl_probe::fetch_wsl_ps_stdout(distro)
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
    fn scan_wsl_observation(&mut self, distro: &str) -> Option<ProcessObservation> {
        match wsl_probe::fetch_wsl_ps_stdout(distro) {
            Some(stdout) => self.observe_wsl_stdout(&stdout, distro),
            None => None,
        }
    }
}

fn is_wsl_program(name: &str) -> bool {
    matches!(
        name.rsplit(['/', '\\'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase()
            .as_str(),
        "wsl" | "wsl.exe"
    )
}

fn shell_label(name: &str) -> Option<String> {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    Some(
        match base.as_str() {
            "bash" => "Bash",
            "zsh" => "Zsh",
            "fish" => "Fish",
            "sh" | "dash" => "sh",
            "pwsh" => "PowerShell",
            "powershell" => "Windows PowerShell",
            "cmd" => "Command Prompt",
            _ => return None,
        }
        .to_string(),
    )
}

fn display_application(name: &str, agent: Option<crate::state_inference::AgentType>) -> String {
    if let Some(agent) = agent {
        return match agent {
            crate::state_inference::AgentType::Claude => "Claude",
            crate::state_inference::AgentType::Codex => "Codex",
            crate::state_inference::AgentType::Aider => "Aider",
            crate::state_inference::AgentType::Copilot => "Copilot",
        }
        .to_string();
    }
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_end_matches(".exe")
        .to_string()
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

    /// `set_wsl_shell_pid` updates the detector's field.
    #[test]
    fn set_wsl_shell_pid_updates() {
        let mut detector = ProcessDetector::new(None);
        assert!(detector.wsl_shell_pid.is_none());
        detector.set_wsl_shell_pid(42);
        assert_eq!(detector.wsl_shell_pid, Some(42));
    }

    /// `scan_wsl` returns an empty vec when `wsl.exe` is missing or fails.
    /// On CI (Linux), `wsl.exe` does not exist so this exercises the
    /// graceful-degradation path where `fetch_wsl_ps_stdout` returns None
    /// and `scan_wsl` returns an empty list instead of panicking.
    #[test]
    fn scan_wsl_returns_empty_on_missing_wsl_exe() {
        let mut detector = ProcessDetector::new(None).with_wsl_distro("NonexistentDistro");
        let agents = detector.scan();
        assert!(
            agents.is_empty(),
            "scan_wsl should return empty when wsl.exe is unavailable"
        );
    }

    /// `scan_if_due` respects the interval even in WSL probe mode.
    #[test]
    fn scan_if_due_respects_interval_in_wsl_mode() {
        let mut detector = ProcessDetector::new(None)
            .with_wsl_distro("Ubuntu")
            .with_interval(Duration::from_secs(60));

        // First scan fires.
        let result = detector.scan_if_due();
        assert!(result.is_some());

        // Second scan within interval is suppressed.
        let result = detector.scan_if_due();
        assert!(result.is_none());
    }

    /// `fetch_wsl_ps_stdout` returns `None` when `wsl.exe` is not
    /// available (non-Windows, or Windows without WSL). Callers rely on
    /// this to distinguish "no agents" from "probe failed".
    #[test]
    fn fetch_wsl_ps_stdout_returns_none_on_missing_wsl() {
        let result = ProcessDetector::fetch_wsl_ps_stdout("Ubuntu");
        // On non-Windows CI, wsl.exe doesn't exist.
        // On Windows without the distro, it returns None.
        // Either way: no panic, returns None.
        if cfg!(not(windows)) {
            assert!(
                result.is_none(),
                "fetch_wsl_ps_stdout should return None on non-Windows"
            );
        }
    }

    /// `classify_wsl_stdout` requires a pane-scoped WSL root and never falls
    /// back to a distro-global attribution.
    #[test]
    fn classify_wsl_stdout_routes_correctly() {
        let stdout = concat!(
            "  100   1 bash -bash\n",
            "  101   1 bash -bash\n",
            "  200 100 node /usr/lib/claude-code/cli.js\n",
            "  300 101 codex codex --resume\n",
        );
        // Without wsl_shell_pid: identity is unknown for this pane.
        let detector = ProcessDetector::new(None).with_wsl_distro("Ubuntu");
        let agents = detector.classify_wsl_stdout(stdout);
        assert!(agents.is_empty());

        // With wsl_shell_pid=100: only claude in shell 100's subtree.
        let mut detector = ProcessDetector::new(None).with_wsl_distro("Ubuntu");
        detector.set_wsl_shell_pid(100);
        let agents = detector.classify_wsl_stdout(stdout);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_type, AgentType::Claude);
        assert_eq!(agents[0].pid, 200);

        // With wsl_shell_pid=101: only codex in shell 101's subtree.
        let mut detector = ProcessDetector::new(None).with_wsl_distro("Ubuntu");
        detector.set_wsl_shell_pid(101);
        let agents = detector.classify_wsl_stdout(stdout);
        assert_eq!(agents.len(), 1);
    }
}
