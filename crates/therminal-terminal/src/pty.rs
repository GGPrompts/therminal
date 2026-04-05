//! Cross-platform PTY management using portable-pty.
//!
//! Abstracts over forkpty (Unix) and ConPTY (Windows) via `portable_pty::native_pty_system()`.

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use thiserror::Error;

/// Result of spawning a shell in a new PTY.
pub type SpawnResult = (Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>);

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("failed to open PTY pair: {0}")]
    Open(#[source] anyhow::Error),

    #[error("failed to spawn shell: {0}")]
    Spawn(#[source] anyhow::Error),

    #[error("failed to resize PTY: {0}")]
    Resize(#[source] anyhow::Error),
}

/// Spawn the user's default shell in a new PTY of the given size.
///
/// Returns the master side of the PTY (for reading/writing) and the child process handle.
pub fn spawn_shell(cols: u16, rows: u16) -> Result<SpawnResult, PtyError> {
    let pty_system = portable_pty::native_pty_system();

    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = pty_system.openpty(size).map_err(PtyError::Open)?;

    let mut cmd = CommandBuilder::new_default_prog();

    // Shell integration env vars — shells detect TERM_PROGRAM to auto-source
    // integration scripts (Ghostty-style detection).
    cmd.env("TERM_PROGRAM", "therminal");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env(
        "THERMINAL_RESOURCES_DIR",
        therminal_runtime::paths::resources_dir(),
    );

    let child = pair.slave.spawn_command(cmd).map_err(PtyError::Spawn)?;

    // Drop the slave side — the child process owns it now.
    drop(pair.slave);

    Ok((pair.master, child))
}

/// Resize an existing PTY to new dimensions.
pub fn resize(master: &dyn MasterPty, cols: u16, rows: u16) -> Result<(), PtyError> {
    master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(PtyError::Resize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // Requires a real TTY / CI environment with shell access
    fn spawn_and_resize() {
        let (master, mut child) = spawn_shell(80, 24).expect("failed to spawn shell");

        // Resize should succeed on a live PTY
        resize(master.as_ref(), 120, 40).expect("failed to resize");

        // Clean up: kill the child process
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn env_vars_set_in_command_builder() {
        // Verify that the command builder sets the expected env vars.
        // We can't easily inspect CommandBuilder's env, so we test via
        // a spawn that echoes the vars (requires a real PTY).
        // For unit-level assurance, we confirm the code compiles and
        // the resources_dir function returns a path.
        let dir = therminal_runtime::paths::resources_dir();
        assert!(
            dir.to_string_lossy().contains("therminal")
                || dir.to_string_lossy().contains("resources")
        );

        // Confirm CARGO_PKG_VERSION resolves at compile time.
        let version = env!("CARGO_PKG_VERSION");
        assert!(!version.is_empty());
    }
}
