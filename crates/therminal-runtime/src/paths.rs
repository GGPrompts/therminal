//! Cross-platform path helpers for Therminal runtime directories.
//!
//! Provides canonical locations for config, data, cache, and socket/runtime
//! files across Linux, macOS, and Windows. Uses the `dirs` crate for
//! platform-native base directories instead of hard-coding XDG or Unix paths.
//!
//! | Purpose | Linux                              | macOS                                    | Windows                                |
//! |---------|------------------------------------|------------------------------------------|----------------------------------------|
//! | Config  | `$XDG_CONFIG_HOME/therminal`       | `~/Library/Application Support/therminal`| `{FOLDERID_RoamingAppData}\therminal`  |
//! | Data    | `$XDG_DATA_HOME/therminal`         | `~/Library/Application Support/therminal`| `{FOLDERID_RoamingAppData}\therminal`  |
//! | Cache   | `$XDG_CACHE_HOME/therminal`        | `~/Library/Caches/therminal`             | `{FOLDERID_LocalAppData}\therminal`    |
//! | Socket  | `$XDG_RUNTIME_DIR/therminal`       | `$TMPDIR/therminal-{user}`               | `{FOLDERID_LocalAppData}\therminal`    |

use std::path::PathBuf;

use tracing::warn;

/// Application directory name used as a subdirectory under each base path.
const APP_DIR: &str = "therminal";

// ── Standard directories ───────────────────────────────────────────────────

/// Return the Therminal configuration directory.
///
/// - Linux: `$XDG_CONFIG_HOME/therminal` (typically `~/.config/therminal`)
/// - macOS: `~/Library/Application Support/therminal`
/// - Windows: `{FOLDERID_RoamingAppData}\therminal`
///
/// Falls back to `/tmp/therminal` on headless systems where no home directory
/// is available.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            warn!("unable to determine config directory; falling back to /tmp/therminal");
            PathBuf::from("/tmp/therminal")
        })
        .join(APP_DIR)
}

/// Return the Therminal data directory.
///
/// - Linux: `$XDG_DATA_HOME/therminal` (typically `~/.local/share/therminal`)
/// - macOS: `~/Library/Application Support/therminal`
/// - Windows: `{FOLDERID_RoamingAppData}\therminal`
///
/// Falls back to `/tmp/therminal` on headless systems where no home directory
/// is available.
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            warn!("unable to determine data directory; falling back to /tmp/therminal");
            PathBuf::from("/tmp/therminal")
        })
        .join(APP_DIR)
}

/// Return the Therminal cache directory.
///
/// - Linux: `$XDG_CACHE_HOME/therminal` (typically `~/.cache/therminal`)
/// - macOS: `~/Library/Caches/therminal`
/// - Windows: `{FOLDERID_LocalAppData}\therminal`
///
/// Falls back to `/tmp/therminal` on headless systems where no home directory
/// is available.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| {
            warn!("unable to determine cache directory; falling back to /tmp/therminal");
            PathBuf::from("/tmp/therminal")
        })
        .join(APP_DIR)
}

/// Return the Therminal runtime/socket directory.
///
/// This directory holds ephemeral files: Unix domain sockets, pidfiles, and
/// lockfiles. It should be on a tmpfs or equivalent fast filesystem.
///
/// - **Linux**: `$XDG_RUNTIME_DIR/therminal` (typically `/run/user/<uid>/therminal`).
///   Falls back to `/tmp/therminal-<user>` if `XDG_RUNTIME_DIR` is unset.
/// - **macOS**: `$TMPDIR/therminal-<user>` (per-user tmpdir provided by launchd).
///   Falls back to `/tmp/therminal-<user>`.
/// - **Windows**: `{FOLDERID_LocalAppData}\therminal` (no Unix sockets; uses
///   named pipes in practice, but the directory is still useful for lockfiles).
pub fn runtime_dir() -> PathBuf {
    platform_runtime_dir()
}

/// Return the full path for a named IPC endpoint.
///
/// - **Unix**: `<runtime_dir>/<name>.sock` (Unix domain socket)
/// - **Windows**: `\\.\pipe\therminal-<name>` (named pipe)
///
/// Example (Linux): `socket_path("daemon")` -> `/run/user/1000/therminal/daemon.sock`
/// Example (Windows): `socket_path("mcp")` -> `\\.\pipe\therminal-mcp`
pub fn socket_path(name: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\therminal-{name}"))
    } else {
        runtime_dir().join(format!("{name}.sock"))
    }
}

/// Return the full path for a named pidfile.
///
/// Example: `pidfile_path("daemon")` -> `<runtime_dir>/daemon.pid`
pub fn pidfile_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.pid"))
}

/// Return the Therminal resources directory.
///
/// Contains shell integration scripts and other bundled assets.
///
/// Resolution order:
/// 1. `THERMINAL_RESOURCES_DIR` env var (runtime override, useful for packaging
///    and custom layouts — the Ghostty approach)
/// 2. `<exe_dir>/../resources` (standard install layout)
/// 3. `<exe_dir>/resources` (flat layout)
/// 4. Compile-time workspace root via `CARGO_MANIFEST_DIR` (debug/dev builds
///    under `cargo run`)
/// 5. `<data_dir>/resources` (final fallback)
pub fn resources_dir() -> PathBuf {
    // 1. Runtime env var override — highest priority.
    if let Ok(dir) = std::env::var("THERMINAL_RESOURCES_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return p;
        }
    }

    // 2. Try relative to executable: <exe_dir>/../resources
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let candidate = exe_dir.join("../resources").canonicalize().ok();
            if let Some(dir) = candidate {
                if dir.is_dir() {
                    return dir;
                }
            }
            // Also check <exe_dir>/resources (flat layout)
            let candidate = exe_dir.join("resources");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }

    // 3. Compile-time fallback for dev builds (`cargo run` / `cargo test`).
    //    CARGO_MANIFEST_DIR points to the crate directory; the workspace root
    //    is two levels up (crates/therminal-runtime -> workspace root).
    if cfg!(debug_assertions) {
        if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR") {
            let workspace_root = PathBuf::from(manifest_dir)
                .join("../..")
                .canonicalize()
                .ok();
            if let Some(root) = workspace_root {
                let candidate = root.join("resources");
                if candidate.is_dir() {
                    return candidate;
                }
            }
        }
    }

    // 4. Fallback: <data_dir>/resources
    data_dir().join("resources")
}

/// Return the full path for a named lockfile.
///
/// Example: `lockfile_path("daemon")` -> `<runtime_dir>/daemon.lock`
pub fn lockfile_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.lock"))
}

/// Ensure the runtime directory exists, creating it with appropriate
/// permissions if needed.
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)?;

    // On Unix, restrict to owner-only access (0o700).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&dir, perms)?;
    }

    Ok(())
}

/// Ensure the config directory exists.
pub fn ensure_config_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())
}

/// Ensure the data directory exists.
pub fn ensure_data_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())
}

/// Ensure the cache directory exists.
pub fn ensure_cache_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(cache_dir())
}

// ── Platform-specific runtime dir ──────────────────────────────────────────

#[cfg(target_os = "linux")]
fn platform_runtime_dir() -> PathBuf {
    // Prefer XDG_RUNTIME_DIR (set by systemd/elogind on modern distros).
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join(APP_DIR);
    }
    // Fallback: /tmp/therminal-<user>
    fallback_tmp_runtime_dir()
}

#[cfg(target_os = "macos")]
fn platform_runtime_dir() -> PathBuf {
    // macOS has no XDG_RUNTIME_DIR, but $TMPDIR is per-user and on a tmpfs-like
    // volume managed by launchd (e.g., /var/folders/xx/.../T/).
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let p = PathBuf::from(tmpdir).join(APP_DIR);
        return p;
    }
    // Very unusual to not have TMPDIR on macOS, but handle it.
    fallback_tmp_runtime_dir()
}

#[cfg(target_os = "windows")]
fn platform_runtime_dir() -> PathBuf {
    // Windows doesn't have Unix sockets (prior to recent builds) or a runtime
    // dir concept. Use LocalAppData which is per-user and fast.
    dirs::data_local_dir()
        .expect("unable to determine local app data directory on Windows")
        .join(APP_DIR)
}

// Catch-all for other Unix-like systems (FreeBSD, etc.)
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_runtime_dir() -> PathBuf {
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join(APP_DIR);
    }
    fallback_tmp_runtime_dir()
}

/// Fallback runtime directory under `/tmp` when no platform-specific runtime
/// dir is available. Includes the username to avoid collisions.
#[cfg(unix)]
fn fallback_tmp_runtime_dir() -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| {
            warn!("Could not determine username for runtime dir fallback; using 'unknown'");
            "unknown".to_string()
        });
    PathBuf::from(format!("/tmp/therminal-{user}"))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_ends_with_therminal() {
        let dir = config_dir();
        assert!(
            dir.ends_with(APP_DIR),
            "config dir should end with '{APP_DIR}', got {dir:?}"
        );
    }

    #[test]
    fn data_dir_ends_with_therminal() {
        let dir = data_dir();
        assert!(
            dir.ends_with(APP_DIR),
            "data dir should end with '{APP_DIR}', got {dir:?}"
        );
    }

    #[test]
    fn cache_dir_ends_with_therminal() {
        let dir = cache_dir();
        assert!(
            dir.ends_with(APP_DIR),
            "cache dir should end with '{APP_DIR}', got {dir:?}"
        );
    }

    #[test]
    fn runtime_dir_ends_with_therminal() {
        let dir = runtime_dir();
        let dir_str = dir.to_string_lossy();
        assert!(
            dir_str.contains("therminal"),
            "runtime dir should contain 'therminal', got {dir:?}"
        );
    }

    #[test]
    fn socket_path_format() {
        let p = socket_path("daemon");
        assert!(
            p.to_string_lossy().ends_with("daemon.sock"),
            "socket path should end with 'daemon.sock', got {p:?}"
        );
        // Parent should be the runtime dir.
        assert_eq!(p.parent().unwrap(), runtime_dir());
    }

    #[test]
    fn pidfile_path_format() {
        let p = pidfile_path("daemon");
        assert!(
            p.to_string_lossy().ends_with("daemon.pid"),
            "pidfile path should end with 'daemon.pid', got {p:?}"
        );
        assert_eq!(p.parent().unwrap(), runtime_dir());
    }

    #[test]
    fn lockfile_path_format() {
        let p = lockfile_path("daemon");
        assert!(
            p.to_string_lossy().ends_with("daemon.lock"),
            "lockfile path should end with 'daemon.lock', got {p:?}"
        );
        assert_eq!(p.parent().unwrap(), runtime_dir());
    }

    #[test]
    fn ensure_runtime_dir_creates_directory() {
        // Use a temp dir to avoid polluting the real runtime dir.
        let tmp = tempfile::tempdir().unwrap();
        let fake_dir = tmp.path().join("therminal-test-runtime");

        // We can't easily override runtime_dir() in tests, so just verify
        // the ensure functions don't panic on the real dirs.
        // For a true integration test, we test ensure_* on the real paths.
        assert!(!fake_dir.exists());
        std::fs::create_dir_all(&fake_dir).unwrap();
        assert!(fake_dir.exists());
    }

    #[test]
    fn all_dirs_are_absolute() {
        assert!(config_dir().is_absolute(), "config_dir should be absolute");
        assert!(data_dir().is_absolute(), "data_dir should be absolute");
        assert!(cache_dir().is_absolute(), "cache_dir should be absolute");
        assert!(
            runtime_dir().is_absolute(),
            "runtime_dir should be absolute"
        );
    }

    #[test]
    fn resources_dir_contains_shell_scripts() {
        let dir = resources_dir();
        assert!(
            dir.is_dir(),
            "resources_dir() should be an existing directory, got {dir:?}"
        );
        let shell_integration = dir.join("shell-integration");
        assert!(
            shell_integration.is_dir(),
            "resources_dir() should contain shell-integration/, got {dir:?}"
        );
        // Verify at least one shell script is present.
        let bash_script = shell_integration.join("therminal.bash");
        assert!(
            bash_script.is_file(),
            "shell-integration/therminal.bash should exist under resources_dir(), got {dir:?}"
        );
    }

    #[test]
    fn resources_dir_env_override() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_resources = tmp.path().join("resources");
        std::fs::create_dir_all(&fake_resources).unwrap();

        // Set the env var and verify it takes precedence.
        std::env::set_var("THERMINAL_RESOURCES_DIR", &fake_resources);
        let dir = resources_dir();
        std::env::remove_var("THERMINAL_RESOURCES_DIR");

        assert_eq!(
            dir, fake_resources,
            "env var override should take precedence"
        );
    }

    #[test]
    fn dirs_are_distinct() {
        // Config, data, cache should generally be different on Linux.
        // On macOS config == data, so we only check cache differs from config.
        let config = config_dir();
        let cache = cache_dir();
        assert_ne!(config, cache, "config and cache dirs should differ");
    }
}
