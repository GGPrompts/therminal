//! File-system watcher for hot-reloading `therminal.toml`.
//!
//! [`ConfigWatcher`] monitors the config file using the `notify` crate and
//! emits [`ConfigChanged`] events through a channel.  Events are debounced
//! to avoid redundant reloads during rapid edits (e.g. save-on-keystroke).

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

use crate::config::TherminalConfig;

// ── Event type ───────────────────────────────────────────────────────────

/// Emitted when the config file changes and is successfully reloaded.
#[derive(Debug, Clone)]
pub struct ConfigChanged {
    /// The newly loaded configuration.
    pub config: TherminalConfig,
}

// ── ConfigWatcher ────────────────────────────────────────────────────────

/// Watches `therminal.toml` and sends [`ConfigChanged`] events on modification.
///
/// The watcher debounces filesystem events (default 500ms) so that rapid
/// successive writes produce at most one reload.
///
/// # Usage
///
/// ```no_run
/// use therminal_core::config_watcher::ConfigWatcher;
///
/// let (watcher, rx) = ConfigWatcher::start().expect("failed to start watcher");
///
/// // In your event loop:
/// while let Ok(event) = rx.try_recv() {
///     println!("Config changed: {:?}", event.config.font.size);
/// }
/// ```
pub struct ConfigWatcher {
    /// Kept alive to maintain the watch. Dropping this stops the watcher.
    _watcher: notify::RecommendedWatcher,
    /// Path being watched.
    path: PathBuf,
}

impl ConfigWatcher {
    /// Start watching the default config path.
    ///
    /// Returns the watcher handle (keep alive) and a receiver for config
    /// change events.
    pub fn start() -> notify::Result<(Self, mpsc::Receiver<ConfigChanged>)> {
        let path = crate::config::config_path();
        Self::start_watching(path)
    }

    /// Start watching a specific config file path.
    pub fn start_watching(path: PathBuf) -> notify::Result<(Self, mpsc::Receiver<ConfigChanged>)> {
        let (tx, rx) = mpsc::channel();
        let config_path = path.clone();

        // We watch the parent directory because many editors (vim, etc.) do
        // atomic writes by creating a temp file then renaming, which means
        // the original inode disappears.  Watching the directory catches
        // Create + Modify + Rename events.
        let watch_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| path.clone());

        // Debounce: collect events for 500ms before processing.
        let debounce = Duration::from_millis(500);
        let (debounce_tx, debounce_rx) = mpsc::channel::<()>();

        let config_path_for_handler = config_path.clone();
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            match res {
                Ok(event) => {
                    let dominated = matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    );
                    if !dominated {
                        return;
                    }

                    // Check if the event involves our config file.
                    let involves_config = event
                        .paths
                        .iter()
                        .any(|p| p.file_name() == config_path_for_handler.file_name());

                    if involves_config {
                        debug!(?event, "config file event");
                        let _ = debounce_tx.send(());
                    }
                }
                Err(e) => {
                    warn!(%e, "config watcher error");
                }
            }
        })?;

        // Ensure the watch directory exists.
        if let Err(e) = std::fs::create_dir_all(&watch_dir) {
            warn!(?watch_dir, %e, "failed to create config directory for watching");
        }

        watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
        info!(?watch_dir, "config watcher started");

        // Spawn debounce thread: waits for events then loads config after
        // a quiet period.
        let config_path_for_thread = config_path.clone();
        std::thread::Builder::new()
            .name("config-watcher-debounce".into())
            .spawn(move || {
                debounce_loop(debounce_rx, tx, config_path_for_thread, debounce);
            })
            .expect("failed to spawn config watcher debounce thread");

        Ok((
            Self {
                _watcher: watcher,
                path: config_path,
            },
            rx,
        ))
    }

    /// The path being watched.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

/// Debounce loop: waits for at least `quiet_period` of silence after the
/// last event before reloading config.
fn debounce_loop(
    events: mpsc::Receiver<()>,
    tx: mpsc::Sender<ConfigChanged>,
    config_path: PathBuf,
    quiet_period: Duration,
) {
    loop {
        // Block until the first event arrives.
        if events.recv().is_err() {
            // Sender dropped (watcher was dropped), exit.
            debug!("config watcher debounce thread exiting");
            return;
        }

        // Drain any additional events within the quiet period.
        loop {
            match events.recv_timeout(quiet_period) {
                Ok(()) => continue,                            // More events, keep waiting.
                Err(mpsc::RecvTimeoutError::Timeout) => break, // Quiet period elapsed.
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }

        // Reload and emit.
        info!(?config_path, "reloading config after file change");
        let config = TherminalConfig::load_from(&config_path);
        if tx.send(ConfigChanged { config }).is_err() {
            // Receiver dropped, exit.
            debug!("config change receiver dropped, watcher exiting");
            return;
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn watcher_detects_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");

        // Write initial config.
        std::fs::write(
            &path,
            r#"
[font]
size = 14.0
"#,
        )
        .unwrap();

        let (_watcher, rx) = ConfigWatcher::start_watching(path.clone()).unwrap();

        // Give the watcher a moment to set up.
        std::thread::sleep(Duration::from_millis(100));

        // Modify the file.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        writeln!(f, "[font]\nsize = 22.0").unwrap();
        f.flush().unwrap();
        drop(f);

        // Wait for the debounced event (500ms debounce + margin).
        let event = rx.recv_timeout(Duration::from_secs(3));
        assert!(event.is_ok(), "expected ConfigChanged event");
        assert_eq!(event.unwrap().config.font.size, 22.0);
    }
}
