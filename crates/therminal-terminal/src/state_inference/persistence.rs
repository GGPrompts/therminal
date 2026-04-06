//! State file I/O for writing agent session state to disk.

use std::path::{Path, PathBuf};

use tracing::{debug, trace, warn};

use super::types::StateFile;

/// Write the current state to a JSON file atomically.
///
/// Returns the updated `(last_write, state_file_path)` on success.
pub(crate) fn write_state_file(
    state_file: &StateFile,
    session_id: &str,
    state_dir: &Path,
) -> Option<PathBuf> {
    // Ensure the state directory exists.
    if !state_dir.exists()
        && let Err(e) = std::fs::create_dir_all(state_dir)
    {
        warn!(dir = %state_dir.display(), error = %e, "Failed to create state directory");
        return None;
    }

    let file_path = state_dir.join(format!("{session_id}.json"));

    // Atomic write: write to .tmp then rename.
    let tmp_path = state_dir.join(format!("{session_id}.json.tmp"));
    match serde_json::to_string_pretty(state_file) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&tmp_path, json.as_bytes()) {
                warn!(path = %tmp_path.display(), error = %e, "Failed to write temp state file");
                return None;
            }
            if let Err(e) = std::fs::rename(&tmp_path, &file_path) {
                warn!(path = %file_path.display(), error = %e, "Failed to rename state file");
                // Clean up tmp file.
                let _ = std::fs::remove_file(&tmp_path);
                return None;
            }
            trace!(
                status = state_file.status.as_str(),
                path = %file_path.display(),
                "Wrote agent state file"
            );
            Some(file_path)
        }
        Err(e) => {
            warn!(error = %e, "Failed to serialize state file");
            None
        }
    }
}

/// Clean up the state file on session exit.
pub(crate) fn cleanup(state_file_path: Option<&PathBuf>) {
    if let Some(path) = state_file_path
        && path.exists()
    {
        if let Err(e) = std::fs::remove_file(path) {
            warn!(path = %path.display(), error = %e, "Failed to remove state file on cleanup");
        } else {
            debug!(path = %path.display(), "Removed state file on session exit");
        }
    }
}

/// Update the state file path after agent type is detected.
pub(crate) fn update_state_file_path(
    agent_type: Option<super::types::AgentType>,
    session_id: &str,
) -> Option<PathBuf> {
    agent_type.map(|at| PathBuf::from(at.state_dir()).join(format!("{session_id}.json")))
}
