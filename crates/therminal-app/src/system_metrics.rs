//! Background system metrics poller (tn-l6y3).
//!
//! Spawns a background thread that periodically samples CPU and memory
//! usage via the `sysinfo` crate and publishes a snapshot through a
//! shared `Arc<Mutex<SystemMetricsSnapshot>>`. The render thread reads
//! the snapshot to format the status bar right section.
//!
//! On Windows native builds with WSL panes, a separate WSL probe
//! (`wsl.exe -e sh -c 'cat /proc/loadavg /proc/meminfo'`) runs at a
//! slower cadence to fetch Linux-side metrics. The probe is cached and
//! only fires when `show_wsl` is enabled. On Linux builds the local
//! sysinfo data *is* the WSL data, so no extra probe is needed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::System;

/// Snapshot of system resource usage at a point in time.
#[derive(Clone, Debug, Default)]
pub struct SystemMetricsSnapshot {
    /// Host (Windows or Linux) CPU usage as a percentage (0.0..100.0).
    pub host_cpu_percent: f32,
    /// Host used memory in bytes.
    pub host_mem_used: u64,
    /// WSL-side load average (1-minute), if available.
    pub wsl_load_avg: Option<f32>,
    /// WSL-side used memory in bytes, if available.
    pub wsl_mem_used: Option<u64>,
    /// Timestamp of the last successful poll.
    pub last_updated: Option<Instant>,
}

impl SystemMetricsSnapshot {
    /// Format the host metrics portion: "38% 7.4G".
    pub fn format_host(&self) -> String {
        let cpu = self.host_cpu_percent;
        let mem = format_bytes_short(self.host_mem_used);
        format!("{cpu:.0}% {mem}")
    }

    /// Format the WSL metrics portion: "0.8 2.1G", or empty if unavailable.
    pub fn format_wsl(&self) -> Option<String> {
        let load = self.wsl_load_avg?;
        let used = self.wsl_mem_used?;
        let mem = format_bytes_short(used);
        Some(format!("{load:.1} {mem}"))
    }

    /// Format the combined status bar text.
    ///
    /// When both host and WSL are available: "Win 38% 7.4G | WSL 0.8 2.1G"
    /// When only host: "38% 7.4G"
    /// When data hasn't been collected yet: empty string.
    pub fn format_status_bar(&self, is_windows_host: bool) -> String {
        if self.last_updated.is_none() {
            return String::new();
        }
        let wsl = self.format_wsl();
        match (is_windows_host, &wsl) {
            (true, Some(wsl_text)) => {
                let host = self.format_host();
                format!("Win {host} | WSL {wsl_text}")
            }
            (true, None) => {
                let host = self.format_host();
                format!("Win {host}")
            }
            (false, _) => {
                // On Linux the host IS the WSL-equivalent environment.
                self.format_host()
            }
        }
    }
}

/// Format bytes into a compact human-readable string (e.g. "7.4G", "512M").
fn format_bytes_short(bytes: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= GB {
        let gb = bytes as f64 / GB as f64;
        if gb >= 10.0 {
            format!("{gb:.0}G")
        } else {
            format!("{gb:.1}G")
        }
    } else {
        let mb = bytes as f64 / MB as f64;
        if mb >= 100.0 {
            format!("{mb:.0}M")
        } else {
            format!("{mb:.1}M")
        }
    }
}

/// Shared handle to the metrics snapshot. Clone-cheap (Arc).
pub type SharedMetrics = Arc<Mutex<SystemMetricsSnapshot>>;

/// Spawn the background metrics poller thread.
///
/// Returns a `SharedMetrics` handle that the render thread should read
/// each frame (the lock is held only briefly during the copy).
///
/// `poll_interval` controls how often the host metrics are sampled.
/// `show_wsl` enables the WSL probe on Windows builds (auto-detected
/// from the environment when set to `None`).
pub fn spawn_metrics_poller(poll_interval: Duration, show_wsl: bool) -> SharedMetrics {
    let shared: SharedMetrics = Arc::new(Mutex::new(SystemMetricsSnapshot::default()));
    let writer = Arc::clone(&shared);

    std::thread::Builder::new()
        .name("system-metrics-poller".into())
        .spawn(move || {
            poller_loop(writer, poll_interval, show_wsl);
        })
        .expect("failed to spawn system-metrics-poller thread");

    shared
}

/// The actual polling loop that runs on the background thread.
fn poller_loop(shared: SharedMetrics, interval: Duration, show_wsl: bool) {
    let mut sys = System::new();

    // sysinfo needs two refresh_cpu_all calls separated by a small
    // delay to produce meaningful usage numbers (the first call is a
    // baseline, the second computes deltas). We do the initial baseline
    // here so the first real poll returns real data.
    sys.refresh_cpu_all();
    std::thread::sleep(Duration::from_millis(200));

    loop {
        // Refresh host CPU + memory.
        sys.refresh_cpu_all();
        sys.refresh_memory();

        let cpu_percent = sys.global_cpu_usage();
        let mem_used = sys.used_memory();

        // WSL probe (Windows-only, gated on show_wsl).
        let (wsl_load, wsl_mem_used, _wsl_mem_total) = if show_wsl {
            probe_wsl_metrics()
        } else {
            (None, None, None)
        };

        // Update the shared snapshot.
        if let Ok(mut snap) = shared.lock() {
            snap.host_cpu_percent = cpu_percent;
            snap.host_mem_used = mem_used;
            snap.wsl_load_avg = wsl_load;
            snap.wsl_mem_used = wsl_mem_used;
            snap.last_updated = Some(Instant::now());
        }

        std::thread::sleep(interval);
    }
}

/// Probe WSL metrics via `wsl.exe -e sh -c 'cat /proc/loadavg; cat /proc/meminfo'`.
///
/// On non-Windows builds, always returns `(None, None, None)`.
/// On Windows, shells out to `wsl.exe` to read `/proc/loadavg` and
/// `/proc/meminfo` from the default distro.
fn probe_wsl_metrics() -> (Option<f32>, Option<u64>, Option<u64>) {
    #[cfg(windows)]
    {
        probe_wsl_metrics_impl()
    }
    #[cfg(not(windows))]
    {
        (None, None, None)
    }
}

/// Actual WSL probe implementation (Windows only).
#[cfg(windows)]
fn probe_wsl_metrics_impl() -> (Option<f32>, Option<u64>, Option<u64>) {
    use std::process::Command;

    let output = match Command::new("wsl.exe")
        .args([
            "-e",
            "sh",
            "-c",
            "cat /proc/loadavg; grep -E '^(MemTotal|MemAvailable):' /proc/meminfo",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return (None, None, None),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    parse_wsl_output(&text)
}

/// Parse the combined output of `/proc/loadavg` + filtered `/proc/meminfo`.
///
/// Expected format:
/// ```text
/// 0.83 0.42 0.21 2/125 12345
/// MemTotal:       16384000 kB
/// MemAvailable:   12000000 kB
/// ```
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_wsl_output(text: &str) -> (Option<f32>, Option<u64>, Option<u64>) {
    let mut load_avg: Option<f32> = None;
    let mut mem_total_kb: Option<u64> = None;
    let mut mem_available_kb: Option<u64> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // /proc/loadavg line: "0.83 0.42 0.21 2/125 12345"
        if load_avg.is_none()
            && !trimmed.starts_with("Mem")
            && let Some(first) = trimmed.split_whitespace().next()
        {
            load_avg = first.parse().ok();
        }

        // MemTotal / MemAvailable lines
        if let Some(rest) = trimmed.strip_prefix("MemTotal:") {
            mem_total_kb = parse_meminfo_value(rest);
        } else if let Some(rest) = trimmed.strip_prefix("MemAvailable:") {
            mem_available_kb = parse_meminfo_value(rest);
        }
    }

    let mem_used = match (mem_total_kb, mem_available_kb) {
        (Some(total), Some(avail)) => Some(total.saturating_sub(avail) * 1024),
        _ => None,
    };
    let mem_total = mem_total_kb.map(|kb| kb * 1024);

    (load_avg, mem_used, mem_total)
}

/// Parse a meminfo value like "  16384000 kB" into kilobytes as u64.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_meminfo_value(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
}

/// Auto-detect whether WSL metrics should be shown.
///
/// Returns `true` on Windows when a WSL distro is detected, or on
/// Linux when `WSL_DISTRO_NAME` is set (meaning we're inside WSL and
/// the host is Windows). Returns `false` in all other cases.
pub fn auto_detect_wsl() -> bool {
    if cfg!(windows) {
        // On Windows native: check if WSL is available.
        // We defer to the existing cached detect_default_distro.
        crate::window::wsl_paths::detect_default_distro().is_some()
    } else {
        // On Linux: if WSL_DISTRO_NAME is set, we're inside WSL.
        // But the status bar should only show WSL metrics when running
        // as Windows native with WSL panes, not when running natively
        // inside WSL. So return false here.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_short_gigabytes() {
        assert_eq!(format_bytes_short(8 * 1024 * 1024 * 1024), "8.0G");
        assert_eq!(format_bytes_short(15 * 1024 * 1024 * 1024), "15G");
        assert_eq!(
            format_bytes_short((7.4 * 1024.0 * 1024.0 * 1024.0) as u64),
            "7.4G"
        );
    }

    #[test]
    fn format_bytes_short_megabytes() {
        assert_eq!(format_bytes_short(512 * 1024 * 1024), "512M");
        assert_eq!(format_bytes_short(64 * 1024 * 1024), "64.0M");
    }

    #[test]
    fn format_status_bar_no_data() {
        let snap = SystemMetricsSnapshot::default();
        assert_eq!(snap.format_status_bar(false), "");
        assert_eq!(snap.format_status_bar(true), "");
    }

    #[test]
    fn format_status_bar_linux_only() {
        let snap = SystemMetricsSnapshot {
            host_cpu_percent: 38.0,
            host_mem_used: (7.4 * 1024.0 * 1024.0 * 1024.0) as u64,
            wsl_load_avg: None,
            wsl_mem_used: None,
            last_updated: Some(Instant::now()),
        };
        assert_eq!(snap.format_status_bar(false), "38% 7.4G");
    }

    #[test]
    fn format_status_bar_windows_no_wsl() {
        let snap = SystemMetricsSnapshot {
            host_cpu_percent: 38.0,
            host_mem_used: (7.4 * 1024.0 * 1024.0 * 1024.0) as u64,
            wsl_load_avg: None,
            wsl_mem_used: None,
            last_updated: Some(Instant::now()),
        };
        assert_eq!(snap.format_status_bar(true), "Win 38% 7.4G");
    }

    #[test]
    fn format_status_bar_windows_with_wsl() {
        let snap = SystemMetricsSnapshot {
            host_cpu_percent: 38.0,
            host_mem_used: (7.4 * 1024.0 * 1024.0 * 1024.0) as u64,
            wsl_load_avg: Some(0.83),
            wsl_mem_used: Some((2.1 * 1024.0 * 1024.0 * 1024.0) as u64),
            last_updated: Some(Instant::now()),
        };
        assert_eq!(snap.format_status_bar(true), "Win 38% 7.4G | WSL 0.8 2.1G");
    }

    #[test]
    fn parse_wsl_output_valid() {
        let text = "\
0.83 0.42 0.21 2/125 12345
MemTotal:       16384000 kB
MemAvailable:   12000000 kB
";
        let (load, used, total) = parse_wsl_output(text);
        assert!((load.unwrap() - 0.83).abs() < 0.01);
        assert_eq!(total.unwrap(), 16384000 * 1024);
        assert_eq!(used.unwrap(), (16384000 - 12000000) * 1024);
    }

    #[test]
    fn parse_wsl_output_empty() {
        let (load, used, total) = parse_wsl_output("");
        assert!(load.is_none());
        assert!(used.is_none());
        assert!(total.is_none());
    }

    #[test]
    fn parse_wsl_output_partial_loadavg_only() {
        let text = "0.50 0.30 0.10 1/50 9999\n";
        let (load, used, total) = parse_wsl_output(text);
        assert!((load.unwrap() - 0.50).abs() < 0.01);
        assert!(used.is_none());
        assert!(total.is_none());
    }
}
