//! Auto-spawn `therminal-daemon` from the GUI when no daemon is running
//! (tn-txs8). Folds tn-6q3v.
//!
//! The GUI is a daemon client by default (tn-beez). Without this helper,
//! a missing daemon means a hard error at startup; this module detects the
//! "not running" case, locates a daemon binary, spawns it detached, and
//! retries the connect for ~1 second.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info};

/// Outcome of the resolution helper. Carries the chain of paths actually
/// inspected so error messages can be specific.
#[derive(Debug)]
pub struct ResolvedBinary {
    pub path: PathBuf,
    pub source: BinarySource,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BinarySource {
    Config,
    NextToCurrentExe,
    Path,
}

/// Filesystem probe abstraction so the resolver can be tested without
/// touching the real filesystem.
pub trait Probe {
    fn exists(&self, p: &Path) -> bool;
    fn which(&self, name: &str) -> Option<PathBuf>;
    fn current_exe_dir(&self) -> Option<PathBuf>;
}

/// Default probe backed by the real filesystem.
pub struct RealProbe;

impl Probe for RealProbe {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn which(&self, name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
    fn current_exe_dir(&self) -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    }
}

/// The basename we look for, with the platform suffix.
pub fn daemon_binary_name() -> &'static str {
    if cfg!(windows) {
        "therminal-daemon.exe"
    } else {
        "therminal-daemon"
    }
}

/// Try to locate a `therminal-daemon` binary using the documented chain.
///
/// 1. `config_override` (from `[daemon] binary_path`) if set and existing.
/// 2. Next to the current executable (`current_exe()/parent()/<name>`).
/// 3. PATH lookup via `which`.
///
/// Returns the first hit and the source bucket. Each tried path is
/// recorded in `searched` for the caller's error message.
pub fn resolve_daemon_binary(
    config_override: Option<&Path>,
    probe: &dyn Probe,
    searched: &mut Vec<PathBuf>,
) -> Option<ResolvedBinary> {
    if let Some(p) = config_override {
        searched.push(p.to_path_buf());
        if probe.exists(p) {
            return Some(ResolvedBinary {
                path: p.to_path_buf(),
                source: BinarySource::Config,
            });
        }
    }

    let name = daemon_binary_name();

    if let Some(dir) = probe.current_exe_dir() {
        let candidate = dir.join(name);
        searched.push(candidate.clone());
        if probe.exists(&candidate) {
            return Some(ResolvedBinary {
                path: candidate,
                source: BinarySource::NextToCurrentExe,
            });
        }
    }

    if let Some(p) = probe.which(name) {
        searched.push(p.clone());
        return Some(ResolvedBinary {
            path: p,
            source: BinarySource::Path,
        });
    } else {
        searched.push(PathBuf::from(format!("$PATH/{name}")));
    }

    None
}

/// Format the "binary not found" error message with the search chain.
pub fn not_found_error_message(searched: &[PathBuf]) -> String {
    let chain = searched
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "therminal daemon binary not found. Searched: {chain}. \
         Install therminal-daemon next to therminal{exe} or set \
         [daemon] binary_path in config.",
        exe = if cfg!(windows) { ".exe" } else { "" },
    )
}

/// Spawn the daemon binary detached from the current process. Stdio is
/// pointed at the null device so it does not race with the GUI's stdout.
pub fn spawn_daemon_detached(binary: &Path) -> Result<()> {
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(binary);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and we only call it from the
        // forked child between fork() and exec(). It detaches the child
        // from the GUI's controlling terminal so it survives our exit.
        unsafe {
            cmd.pre_exec(|| {
                let _ = libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()
        .with_context(|| format!("failed to spawn {}", binary.display()))?;
    Ok(())
}

/// Determine whether a connect-error indicates "no daemon listening".
///
/// Unix: ENOENT (socket file missing) or ECONNREFUSED (no listener bound).
/// Windows: ERROR_FILE_NOT_FOUND (2) when the named pipe does not exist.
pub fn is_not_running_error(err: &anyhow::Error) -> bool {
    // Walk the error chain looking for an io::Error.
    for cause in err.chain() {
        if let Some(ioe) = cause.downcast_ref::<std::io::Error>() {
            match ioe.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    return true;
                }
                _ => {}
            }
            #[cfg(windows)]
            {
                // ERROR_FILE_NOT_FOUND
                if ioe.raw_os_error() == Some(2) {
                    return true;
                }
            }
        }
    }
    false
}

/// Run the auto-spawn workflow: locate the binary, spawn it, and wait
/// briefly for the daemon socket to come up. The caller is expected to
/// retry its connect after this returns Ok.
pub fn auto_spawn(config_override: Option<&Path>) -> Result<PathBuf> {
    let mut searched = Vec::new();
    let resolved = resolve_daemon_binary(config_override, &RealProbe, &mut searched)
        .ok_or_else(|| anyhow::anyhow!("{}", not_found_error_message(&searched)))?;

    info!(
        path = %resolved.path.display(),
        source = ?resolved.source,
        "auto-spawning therminal-daemon"
    );
    spawn_daemon_detached(&resolved.path)?;
    Ok(resolved.path)
}

/// Poll a connect closure for ~1s to give a freshly-spawned daemon time
/// to bind its socket. Logs each attempt at debug level.
pub fn retry_connect<F, T>(mut connect: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    const ATTEMPTS: u32 = 10;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(100);

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=ATTEMPTS {
        match connect() {
            Ok(v) => {
                debug!(attempt, "connected to daemon after auto-spawn");
                return Ok(v);
            }
            Err(e) => {
                debug!(attempt, error = %e, "connect retry failed");
                last_err = Some(e);
                std::thread::sleep(DELAY);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("retry_connect exhausted with no error")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeProbe {
        existing: Vec<PathBuf>,
        path_hit: Option<PathBuf>,
        exe_dir: Option<PathBuf>,
        which_calls: RefCell<Vec<String>>,
    }

    impl Probe for FakeProbe {
        fn exists(&self, p: &Path) -> bool {
            self.existing.iter().any(|e| e == p)
        }
        fn which(&self, name: &str) -> Option<PathBuf> {
            self.which_calls.borrow_mut().push(name.to_string());
            self.path_hit.clone()
        }
        fn current_exe_dir(&self) -> Option<PathBuf> {
            self.exe_dir.clone()
        }
    }

    #[test]
    fn resolves_config_override_first() {
        let cfg = PathBuf::from("/opt/custom/therminal-daemon");
        let probe = FakeProbe {
            existing: vec![cfg.clone()],
            path_hit: None,
            exe_dir: None,
            which_calls: RefCell::new(vec![]),
        };
        let mut searched = vec![];
        let r = resolve_daemon_binary(Some(&cfg), &probe, &mut searched).unwrap();
        assert_eq!(r.source, BinarySource::Config);
        assert_eq!(r.path, cfg);
    }

    #[test]
    fn resolves_next_to_current_exe() {
        let dir = PathBuf::from("/usr/local/bin");
        let candidate = dir.join(daemon_binary_name());
        let probe = FakeProbe {
            existing: vec![candidate.clone()],
            path_hit: None,
            exe_dir: Some(dir),
            which_calls: RefCell::new(vec![]),
        };
        let mut searched = vec![];
        let r = resolve_daemon_binary(None, &probe, &mut searched).unwrap();
        assert_eq!(r.source, BinarySource::NextToCurrentExe);
        assert_eq!(r.path, candidate);
    }

    #[test]
    fn falls_back_to_path_lookup() {
        let path_hit = PathBuf::from("/home/me/.cargo/bin").join(daemon_binary_name());
        let probe = FakeProbe {
            existing: vec![],
            path_hit: Some(path_hit.clone()),
            exe_dir: Some(PathBuf::from("/tmp")),
            which_calls: RefCell::new(vec![]),
        };
        let mut searched = vec![];
        let r = resolve_daemon_binary(None, &probe, &mut searched).unwrap();
        assert_eq!(r.source, BinarySource::Path);
        assert_eq!(r.path, path_hit);
    }

    #[test]
    fn not_found_collects_search_chain() {
        let cfg = PathBuf::from("/opt/missing/therminal-daemon");
        let probe = FakeProbe {
            existing: vec![],
            path_hit: None,
            exe_dir: Some(PathBuf::from("/tmp")),
            which_calls: RefCell::new(vec![]),
        };
        let mut searched = vec![];
        assert!(resolve_daemon_binary(Some(&cfg), &probe, &mut searched).is_none());
        assert_eq!(searched.len(), 3);
        let msg = not_found_error_message(&searched);
        assert!(msg.contains("/opt/missing/therminal-daemon"));
        assert!(msg.contains("[daemon] binary_path"));
        assert!(msg.contains("Install therminal-daemon"));
    }

    #[test]
    fn detects_not_running_io_errors() {
        let nf: anyhow::Error = std::io::Error::from(std::io::ErrorKind::NotFound).into();
        assert!(is_not_running_error(&nf));
        let cr: anyhow::Error = std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into();
        assert!(is_not_running_error(&cr));
        let other: anyhow::Error =
            std::io::Error::from(std::io::ErrorKind::PermissionDenied).into();
        assert!(!is_not_running_error(&other));
    }
}
