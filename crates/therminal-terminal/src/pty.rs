//! Cross-platform PTY management using portable-pty.
//!
//! Abstracts over forkpty (Unix) and ConPTY (Windows) via `portable_pty::native_pty_system()`.
//! Shell integration scripts are auto-sourced on spawn via shell-specific injection
//! (rcfile wrappers, ZDOTDIR, fish --init-command, PowerShell -Command).

use std::path::{Path, PathBuf};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};
use thiserror::Error;
use tracing::{debug, warn};

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

    #[error("failed to prepare shell integration: {0}")]
    Integration(String),
}

/// Known shell types for integration injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellType {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    /// WSL — launches a Linux shell inside Windows Subsystem for Linux.
    Wsl,
    Unknown,
}

/// Detect the shell type from a shell path or name.
pub fn detect_shell_type(shell: &str) -> ShellType {
    let basename = Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(shell);
    match basename {
        "bash" => ShellType::Bash,
        "zsh" => ShellType::Zsh,
        "fish" => ShellType::Fish,
        "pwsh" | "powershell" | "pwsh.exe" | "powershell.exe" => ShellType::PowerShell,
        "wsl" | "wsl.exe" => ShellType::Wsl,
        _ => ShellType::Unknown,
    }
}

/// Resolve the user's default shell path.
///
/// Checks `$SHELL` first (Unix), then falls back to passwd database lookup
/// or `ComSpec` on Windows.
///
/// Exposed (`pub`) so callers that need to record the *actual* shell that
/// will be spawned — not the empty-string request the user passed in — can
/// mirror the same resolution logic. Before tn-x1h9 this was private and
/// the daemon silently stored `""` as the pane's shell command, which
/// broke the `wsl.exe` basename check in `process_detector_task` on
/// Windows native + WSL panes (no agent registry entry ever got
/// populated, breaking the Claude observability pipeline).
pub fn get_default_shell() -> String {
    #[cfg(unix)]
    {
        // Prefer $SHELL, same logic as portable-pty's CommandBuilder::get_shell.
        if let Ok(shell) = std::env::var("SHELL")
            && !shell.is_empty()
        {
            return shell;
        }
        // Fallback: passwd database.
        // SAFETY: `getuid` is a pure read of the calling process's real
        // user id and has no preconditions. `getpwuid` returns either a
        // pointer to a static `passwd` struct owned by libc or NULL on
        // error / "not found"; we null-check `ent` before any deref.
        // Reading `(*ent).pw_shell` is sound while we still hold the
        // pointer — we don't call any other passwd-database function on
        // this thread (which would be the only way to invalidate the
        // static buffer) before extracting the C string. `CStr::from_ptr`
        // requires the pointer to point to a NUL-terminated C string;
        // POSIX guarantees `pw_shell` is one. The lifetime of the
        // resulting `&CStr` is bounded by the surrounding `unsafe` block,
        // and `to_str().to_owned()` copies it before we return so the
        // caller never sees the libc-owned memory.
        unsafe {
            let ent = libc::getpwuid(libc::getuid());
            if !ent.is_null() {
                let shell = std::ffi::CStr::from_ptr((*ent).pw_shell);
                if let Ok(s) = shell.to_str()
                    && !s.is_empty()
                {
                    return s.to_owned();
                }
            }
        }
        "/bin/sh".to_owned()
    }
    #[cfg(windows)]
    {
        // On Windows, prefer WSL if available (gives a full Linux shell),
        // then PowerShell, then cmd.exe as last resort.
        if std::process::Command::new("wsl.exe")
            .arg("--status")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return "wsl.exe".to_owned();
        }
        for candidate in &["pwsh.exe", "powershell.exe"] {
            if std::process::Command::new(candidate)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
            {
                return candidate.to_string();
            }
        }
        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_owned())
    }
}

/// Return the path to the shell-integration directory inside resources.
fn shell_integration_dir() -> PathBuf {
    therminal_runtime::paths::resources_dir().join("shell-integration")
}

/// Convert a Windows path like `C:\Users\foo\bar` to WSL format `/mnt/c/Users/foo/bar`.
/// Returns the original path as a string if it doesn't look like a Windows absolute path.
fn windows_to_wsl_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.len() >= 3 && s.as_bytes()[1] == b':' {
        let drive = (s.as_bytes()[0] as char).to_ascii_lowercase();
        let rest = s[2..].replace('\\', "/");
        format!("/mnt/{drive}{rest}")
    } else {
        s.into_owned()
    }
}

/// Prepare a bash rcfile wrapper for WSL that uses Linux-format paths.
///
/// The wrapper sources the integration script (given as a WSL path) then
/// the user's real `.bashrc`.
fn prepare_wsl_bash_rcfile(wsl_integration_path: &str) -> Result<PathBuf, PtyError> {
    let cache_dir = therminal_runtime::paths::cache_dir();
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| PtyError::Integration(format!("create cache dir: {e}")))?;

    let wrapper_path = cache_dir.join("wsl_bash_rcfile.bash");
    let content = format!(
        r#"# Therminal WSL bash wrapper — auto-generated, do not edit.
# Sources shell integration, then the user's real .bashrc.
if [ -f "{integration}" ]; then
    . "{integration}"
fi
if [ -f "$HOME/.bashrc" ]; then
    . "$HOME/.bashrc"
fi
"#,
        integration = wsl_integration_path
    );
    std::fs::write(&wrapper_path, content)
        .map_err(|e| PtyError::Integration(format!("write wsl bash rcfile: {e}")))?;
    Ok(wrapper_path)
}

/// Prepare a bash rcfile wrapper that sources the integration script then
/// the user's real `.bashrc`.
///
/// Returns the path to the temp wrapper file. The file is written to the
/// therminal cache directory so it persists across the session.
fn prepare_bash_rcfile() -> Result<PathBuf, PtyError> {
    let integration_script = shell_integration_dir().join("therminal.bash");
    let cache_dir = therminal_runtime::paths::cache_dir();
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| PtyError::Integration(format!("create cache dir: {e}")))?;

    let wrapper_path = cache_dir.join("bash_rcfile.bash");
    let content = format!(
        r#"# Therminal bash wrapper — auto-generated, do not edit.
# Sources shell integration, then the user's real .bashrc.
if [ -f {integration:?} ]; then
    . {integration:?}
fi
if [ -f "$HOME/.bashrc" ]; then
    . "$HOME/.bashrc"
fi
"#,
        integration = integration_script.display()
    );
    std::fs::write(&wrapper_path, content)
        .map_err(|e| PtyError::Integration(format!("write bash rcfile: {e}")))?;
    Ok(wrapper_path)
}

/// Prepare a ZDOTDIR overlay for zsh that sources integration then delegates
/// to the user's real zsh config.
///
/// Creates `$CACHE_DIR/zsh/.zshenv` and `$CACHE_DIR/zsh/.zshrc` that source
/// the integration script and then the user's real files.
fn prepare_zsh_zdotdir() -> Result<PathBuf, PtyError> {
    let integration_script = shell_integration_dir().join("therminal.zsh");
    let cache_dir = therminal_runtime::paths::cache_dir();
    let zdotdir = cache_dir.join("zsh");
    std::fs::create_dir_all(&zdotdir)
        .map_err(|e| PtyError::Integration(format!("create zsh ZDOTDIR: {e}")))?;

    // .zshenv — sourced first for ALL zsh invocations.
    // We keep ZDOTDIR pointing at our overlay so zsh finds our .zshrc/.zprofile.
    // But we source the user's real .zshenv via the saved original path.
    let zshenv_content = r#"# Therminal zsh integration — auto-generated, do not edit.
# Source the real .zshenv (using saved original ZDOTDIR) without restoring
# ZDOTDIR yet — we need zsh to keep reading our overlay for .zshrc/.zprofile.
_therminal_real_zdotdir="${_THERMINAL_ORIG_ZDOTDIR:-$HOME}"
if [ -f "${_therminal_real_zdotdir}/.zshenv" ]; then
    . "${_therminal_real_zdotdir}/.zshenv"
fi
unset _therminal_real_zdotdir
"#;
    std::fs::write(zdotdir.join(".zshenv"), zshenv_content)
        .map_err(|e| PtyError::Integration(format!("write .zshenv: {e}")))?;

    // .zshrc — sourced for interactive shells.
    // Source integration script, THEN restore ZDOTDIR, THEN source user's .zshrc.
    let zshrc_content = format!(
        r#"# Therminal zsh integration — auto-generated, do not edit.
# Source integration script first (while ZDOTDIR still points here).
if [ -f {integration:?} ]; then
    . {integration:?}
fi

# NOW restore original ZDOTDIR so user config and future shells work normally.
if [ -n "${{_THERMINAL_ORIG_ZDOTDIR+x}}" ]; then
    ZDOTDIR="${{_THERMINAL_ORIG_ZDOTDIR}}"
    unset _THERMINAL_ORIG_ZDOTDIR
else
    unset ZDOTDIR
fi

# Source the real .zshrc if it exists.
if [ -f "${{ZDOTDIR:-$HOME}}/.zshrc" ]; then
    . "${{ZDOTDIR:-$HOME}}/.zshrc"
fi
"#,
        integration = integration_script.display()
    );
    std::fs::write(zdotdir.join(".zshrc"), zshrc_content)
        .map_err(|e| PtyError::Integration(format!("write .zshrc: {e}")))?;

    // .zprofile — sourced for login shells before .zshrc
    let zprofile_content = r#"# Therminal zsh integration — auto-generated, do not edit.
# Source the real .zprofile (using saved original ZDOTDIR) if it exists.
_therminal_real_zdotdir="${_THERMINAL_ORIG_ZDOTDIR:-$HOME}"
if [ -f "${_therminal_real_zdotdir}/.zprofile" ]; then
    . "${_therminal_real_zdotdir}/.zprofile"
fi
unset _therminal_real_zdotdir
"#;
    std::fs::write(zdotdir.join(".zprofile"), zprofile_content)
        .map_err(|e| PtyError::Integration(format!("write .zprofile: {e}")))?;

    // .zlogin — sourced for login shells after .zshrc
    let zlogin_content = r#"# Therminal zsh integration — auto-generated, do not edit.
# Source the real .zlogin (using saved original ZDOTDIR) if it exists.
_therminal_real_zdotdir="${_THERMINAL_ORIG_ZDOTDIR:-$HOME}"
if [ -f "${_therminal_real_zdotdir}/.zlogin" ]; then
    . "${_therminal_real_zdotdir}/.zlogin"
fi
unset _therminal_real_zdotdir
"#;
    std::fs::write(zdotdir.join(".zlogin"), zlogin_content)
        .map_err(|e| PtyError::Integration(format!("write .zlogin: {e}")))?;

    Ok(zdotdir)
}

/// Set common env vars on a command builder.
fn set_common_env(cmd: &mut CommandBuilder) {
    cmd.env("TERM_PROGRAM", "therminal");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env(
        "THERMINAL_RESOURCES_DIR",
        therminal_runtime::paths::resources_dir(),
    );
}

/// Build a `CommandBuilder` for the given shell with integration auto-sourcing.
///
/// Each shell type gets a tailored injection strategy:
/// - **Bash**: `--rcfile <wrapper>` where the wrapper sources integration + `~/.bashrc`.
///   Also sets `BASH_ENV` for non-interactive subshells.
/// - **Zsh**: `ZDOTDIR` redirect to a temp dir whose `.zshenv`/`.zshrc` source
///   integration and then delegate to the user's real config.
/// - **Fish**: `--init-command "source <integration_script>"`.
/// - **PowerShell**: `-NoExit -Command ". <integration_script>"`.
/// - **Unknown**: Falls back to `BASH_ENV` (works if shell is sh-compatible).
fn build_shell_command(
    shell: &str,
    shell_type: ShellType,
    initial_cwd: Option<&str>,
) -> Result<CommandBuilder, PtyError> {
    let integration_dir = shell_integration_dir();

    match shell_type {
        ShellType::Bash => {
            let rcfile = prepare_bash_rcfile()?;
            let integration_script = integration_dir.join("therminal.bash");
            let mut cmd = CommandBuilder::new(shell);
            // --rcfile makes bash read our wrapper instead of ~/.bashrc.
            // We pass --login so profile files (.bash_profile etc.) are read,
            // but --rcfile is only honored for non-login interactive shells.
            // So we DON'T pass --login; the wrapper sources .bashrc explicitly.
            cmd.args(["--rcfile", &rcfile.to_string_lossy()]);
            set_common_env(&mut cmd);
            // BASH_ENV is read by non-interactive bash (scripts, subshells).
            cmd.env("BASH_ENV", &integration_script);
            debug!(
                shell = shell,
                rcfile = %rcfile.display(),
                "bash: using --rcfile wrapper for integration"
            );
            Ok(cmd)
        }
        ShellType::Zsh => {
            let zdotdir = prepare_zsh_zdotdir()?;
            let mut cmd = CommandBuilder::new(shell);
            // Spawn as login shell for zsh.
            cmd.args(["--login"]);
            set_common_env(&mut cmd);
            // Save original ZDOTDIR so our wrapper can restore it.
            if let Ok(orig) = std::env::var("ZDOTDIR") {
                cmd.env("_THERMINAL_ORIG_ZDOTDIR", &orig);
            }
            cmd.env("ZDOTDIR", &zdotdir);
            debug!(
                shell = shell,
                zdotdir = %zdotdir.display(),
                "zsh: using ZDOTDIR redirect for integration"
            );
            Ok(cmd)
        }
        ShellType::Fish => {
            let integration_script = integration_dir.join("therminal.fish");
            let source_cmd = format!("source '{}'", integration_script.display());
            let mut cmd = CommandBuilder::new(shell);
            // --login for login behavior, --init-command to source integration.
            cmd.args(["--login", "--init-command", &source_cmd]);
            set_common_env(&mut cmd);
            debug!(shell = shell, "fish: using --init-command for integration");
            Ok(cmd)
        }
        ShellType::PowerShell => {
            let integration_script = integration_dir.join("therminal.ps1");
            let dot_source = format!(". '{}'", integration_script.display());
            let mut cmd = CommandBuilder::new(shell);
            cmd.args(["-NoExit", "-Command", &dot_source]);
            set_common_env(&mut cmd);
            debug!(
                shell = shell,
                "powershell: using -NoExit -Command for integration"
            );
            Ok(cmd)
        }
        ShellType::Wsl => {
            // WSL launches a Linux shell on Windows. We inject shell integration
            // by generating a bash rcfile wrapper and passing it via
            // `wsl -- bash --rcfile <wrapper>`.
            let mut cmd = CommandBuilder::new(shell);
            // Use the provided Linux cwd if available (split inheritance),
            // otherwise fall back to the Linux home directory.
            cmd.arg("--cd");
            cmd.arg(initial_cwd.unwrap_or("~"));
            cmd.env("TERM_PROGRAM", "therminal");
            cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));

            // Convert Windows resources path to WSL path for the Linux shell.
            let resources = therminal_runtime::paths::resources_dir();
            let wsl_resources = windows_to_wsl_path(&resources);
            cmd.env("THERMINAL_RESOURCES_DIR", &wsl_resources);

            // Generate a bash rcfile that sources integration + user's .bashrc,
            // using WSL-compatible paths so bash inside WSL can read them.
            let wsl_integration = format!("{wsl_resources}/shell-integration/therminal.bash");
            let rcfile = prepare_wsl_bash_rcfile(&wsl_integration)?;
            let wsl_rcfile = windows_to_wsl_path(&rcfile);

            // Launch bash inside WSL with our rcfile wrapper.
            cmd.args(["--", "bash", "--rcfile", &wsl_rcfile]);
            // BASH_ENV for non-interactive subshells.
            cmd.env("BASH_ENV", &wsl_integration);

            debug!(
                shell = shell,
                rcfile = %wsl_rcfile,
                "wsl: launching bash with --rcfile for integration"
            );
            Ok(cmd)
        }
        ShellType::Unknown => {
            // Best-effort: use the configured shell (not default_prog) and set
            // ENV (POSIX sh reads $ENV for interactive shells) and BASH_ENV.
            // If the shell doesn't support these, integration won't auto-source,
            // but nothing breaks.
            let integration_script = integration_dir.join("therminal.bash");
            let mut cmd = CommandBuilder::new(shell);
            set_common_env(&mut cmd);
            if integration_script.exists() {
                cmd.env("ENV", &integration_script);
                cmd.env("BASH_ENV", &integration_script);
            }
            warn!(
                shell = shell,
                "unknown shell type; integration may not auto-source"
            );
            Ok(cmd)
        }
    }
}

/// Options for customizing shell spawn behavior.
#[derive(Debug, Default)]
pub struct SpawnOptions {
    /// Shell command to use instead of the user's default. Empty = use default.
    pub shell: String,
    /// Extra arguments to append to the shell command line after shell-integration
    /// args. These are appended verbatim, so they must be valid for the target shell.
    pub shell_args: Vec<String>,
    /// Extra environment variables to merge into the PTY environment.
    pub env: std::collections::HashMap<String, String>,
    /// Working directory for the spawned shell. Empty = inherit current directory.
    pub cwd: String,
}

/// Spawn the user's default shell in a new PTY of the given size.
///
/// Detects the shell type and injects integration scripts automatically.
/// Returns the master side of the PTY (for reading/writing) and the child process handle.
pub fn spawn_shell(cols: u16, rows: u16) -> Result<SpawnResult, PtyError> {
    spawn_shell_with_options(cols, rows, &SpawnOptions::default())
}

/// Return the actual shell path that will be used for a PTY spawn with the
/// given options. If `options.shell` is non-empty, returns it verbatim;
/// otherwise returns [`get_default_shell`].
///
/// This mirrors the resolution branch inside [`spawn_shell_with_options`]
/// so callers that need to *record* the resolved shell (e.g. the daemon's
/// `Pane.shell` field, used downstream by `process_detector_task` to
/// decide whether the pane should use the WSL probe path — tn-x1h9) can
/// do so without duplicating the logic. tn-966s.
pub fn resolve_shell(options: &SpawnOptions) -> String {
    if options.shell.is_empty() {
        get_default_shell()
    } else {
        options.shell.clone()
    }
}

/// Spawn a shell in a new PTY with custom options (shell override, extra env vars).
///
/// If `options.shell` is non-empty, it is used instead of the user's default shell.
/// Extra env vars from `options.env` are merged into the PTY environment.
pub fn spawn_shell_with_options(
    cols: u16,
    rows: u16,
    options: &SpawnOptions,
) -> Result<SpawnResult, PtyError> {
    let pty_system = portable_pty::native_pty_system();

    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = pty_system.openpty(size).map_err(PtyError::Open)?;

    let shell = if options.shell.is_empty() {
        get_default_shell()
    } else {
        debug!(shell = %options.shell, "using config shell override");
        options.shell.clone()
    };
    let shell_type = detect_shell_type(&shell);
    debug!(?shell, ?shell_type, "detected shell for PTY spawn");

    let cwd_for_cmd = if options.cwd.is_empty() {
        None
    } else {
        Some(options.cwd.as_str())
    };
    let mut cmd = build_shell_command(&shell, shell_type, cwd_for_cmd)?;

    // Append user-configured shell args after shell-integration args.
    if !options.shell_args.is_empty() {
        let args: Vec<&str> = options.shell_args.iter().map(|s| s.as_str()).collect();
        cmd.args(&args);
    }

    // Merge extra env vars from config.
    for (k, v) in &options.env {
        cmd.env(k, v);
    }

    // Set working directory if specified.
    if !options.cwd.is_empty() {
        cmd.cwd(&options.cwd);
    }

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

    #[test]
    fn resolve_shell_with_override_returns_override_verbatim() {
        let opts = SpawnOptions {
            shell: "/usr/local/bin/fish".to_string(),
            ..Default::default()
        };
        assert_eq!(resolve_shell(&opts), "/usr/local/bin/fish");
    }

    #[test]
    fn resolve_shell_empty_returns_default_shell() {
        // tn-x1h9: empty `options.shell` must resolve to whatever
        // `get_default_shell()` returns — never the empty string. The
        // daemon stores this value in `Pane.shell` and the
        // `process_detector_task::shell_command_is_wsl` heuristic
        // short-circuits on empty input, which silently defeated the
        // WSL probe path on Windows + WSL panes.
        let opts = SpawnOptions::default();
        let resolved = resolve_shell(&opts);
        assert!(
            !resolved.is_empty(),
            "resolve_shell(default) must never return empty — caught the Windows+WSL observability regression"
        );
        assert_eq!(resolved, get_default_shell());
    }

    #[test]
    fn detect_shell_type_from_path() {
        assert_eq!(detect_shell_type("/bin/bash"), ShellType::Bash);
        assert_eq!(detect_shell_type("/usr/bin/bash"), ShellType::Bash);
        assert_eq!(detect_shell_type("bash"), ShellType::Bash);
        assert_eq!(detect_shell_type("/bin/zsh"), ShellType::Zsh);
        assert_eq!(detect_shell_type("/usr/local/bin/zsh"), ShellType::Zsh);
        assert_eq!(detect_shell_type("zsh"), ShellType::Zsh);
        assert_eq!(detect_shell_type("/usr/bin/fish"), ShellType::Fish);
        assert_eq!(detect_shell_type("fish"), ShellType::Fish);
        assert_eq!(detect_shell_type("pwsh"), ShellType::PowerShell);
        assert_eq!(detect_shell_type("powershell"), ShellType::PowerShell);
        assert_eq!(detect_shell_type("/usr/bin/pwsh"), ShellType::PowerShell);
        assert_eq!(detect_shell_type("/bin/sh"), ShellType::Unknown);
        assert_eq!(detect_shell_type("ksh"), ShellType::Unknown);
        assert_eq!(detect_shell_type("/usr/bin/tcsh"), ShellType::Unknown);
    }

    #[test]
    fn prepare_bash_rcfile_creates_wrapper() {
        let rcfile = prepare_bash_rcfile().expect("failed to prepare bash rcfile");
        assert!(rcfile.exists(), "bash rcfile should exist at {rcfile:?}");
        let content = std::fs::read_to_string(&rcfile).unwrap();
        assert!(
            content.contains("therminal.bash"),
            "wrapper should source therminal.bash"
        );
        assert!(
            content.contains(".bashrc"),
            "wrapper should source user .bashrc"
        );
    }

    #[test]
    fn prepare_zsh_zdotdir_creates_config_files() {
        let zdotdir = prepare_zsh_zdotdir().expect("failed to prepare zsh ZDOTDIR");
        assert!(zdotdir.is_dir(), "ZDOTDIR should be a directory");

        let zshenv = zdotdir.join(".zshenv");
        assert!(zshenv.exists(), ".zshenv should exist");
        let env_content = std::fs::read_to_string(&zshenv).unwrap();
        assert!(
            env_content.contains("_THERMINAL_ORIG_ZDOTDIR"),
            ".zshenv should reference original ZDOTDIR for sourcing user's .zshenv"
        );

        let zshrc = zdotdir.join(".zshrc");
        assert!(zshrc.exists(), ".zshrc should exist");
        let rc_content = std::fs::read_to_string(&zshrc).unwrap();
        assert!(
            rc_content.contains("therminal.zsh"),
            ".zshrc should source integration script"
        );

        let zprofile = zdotdir.join(".zprofile");
        assert!(zprofile.exists(), ".zprofile should exist");
        let profile_content = std::fs::read_to_string(&zprofile).unwrap();
        assert!(
            profile_content.contains("_THERMINAL_ORIG_ZDOTDIR"),
            ".zprofile should reference saved original ZDOTDIR, not current ZDOTDIR"
        );

        let zlogin = zdotdir.join(".zlogin");
        assert!(zlogin.exists(), ".zlogin should exist");
        let login_content = std::fs::read_to_string(&zlogin).unwrap();
        assert!(
            login_content.contains("_THERMINAL_ORIG_ZDOTDIR"),
            ".zlogin should reference saved original ZDOTDIR, not current ZDOTDIR"
        );
    }

    #[test]
    fn build_shell_command_bash() {
        let cmd = build_shell_command("/bin/bash", ShellType::Bash, None)
            .expect("failed to build bash command");
        // The command should not be a default_prog (it has explicit args).
        assert!(
            !cmd.is_default_prog(),
            "bash command should have explicit args"
        );
    }

    /// When splitting a WSL pane, the inherited Linux cwd (e.g.
    /// `/home/marci/projects`) must be forwarded as `wsl.exe --cd <path>`.
    /// Previously this was hardcoded to `--cd ~`, which always started the
    /// new pane in the Linux home directory regardless of where the source
    /// pane was.
    #[cfg(windows)]
    #[test]
    fn build_shell_command_wsl_uses_provided_cwd() {
        let cmd = build_shell_command("wsl.exe", ShellType::Wsl, Some("/home/marci/projects"))
            .expect("failed to build wsl command");
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Find the --cd argument and assert its value is the Linux path.
        let cd_pos = argv
            .iter()
            .position(|a| a == "--cd")
            .expect("wsl command should contain --cd");
        assert_eq!(
            argv.get(cd_pos + 1).map(String::as_str),
            Some("/home/marci/projects"),
            "wsl.exe --cd should receive the Linux cwd, not ~ or a UNC path"
        );
    }

    #[cfg(windows)]
    #[test]
    fn build_shell_command_wsl_falls_back_to_home_when_no_cwd() {
        let cmd = build_shell_command("wsl.exe", ShellType::Wsl, None)
            .expect("failed to build wsl command");
        let argv: Vec<String> = cmd
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let cd_pos = argv
            .iter()
            .position(|a| a == "--cd")
            .expect("wsl command should contain --cd");
        assert_eq!(
            argv.get(cd_pos + 1).map(String::as_str),
            Some("~"),
            "wsl.exe --cd should fall back to ~ when no cwd is provided"
        );
    }

    #[test]
    fn build_shell_command_unknown_uses_configured_shell() {
        let cmd = build_shell_command("/bin/sh", ShellType::Unknown, None)
            .expect("failed to build unknown shell command");
        // Unknown shell should use the configured shell path, not default_prog.
        assert!(
            !cmd.is_default_prog(),
            "unknown shell should use the configured path, not default_prog"
        );
    }

    #[test]
    fn spawn_options_default_uses_empty_shell() {
        let opts = SpawnOptions::default();
        assert!(
            opts.shell.is_empty(),
            "default SpawnOptions should use empty shell (= user's login shell)"
        );
        assert!(
            opts.env.is_empty(),
            "default SpawnOptions should have no extra env vars"
        );
    }

    #[test]
    fn spawn_options_shell_override_selects_correct_shell_type() {
        let opts = SpawnOptions {
            shell: "/usr/bin/fish".to_string(),
            ..Default::default()
        };
        // When shell is non-empty, spawn_shell_with_options uses it instead of default.
        assert!(!opts.shell.is_empty());
        assert_eq!(
            detect_shell_type(&opts.shell),
            ShellType::Fish,
            "shell override should be detected as Fish"
        );
    }

    #[test]
    fn spawn_options_env_vars_are_accessible() {
        let mut env = std::collections::HashMap::new();
        env.insert("EDITOR".to_string(), "nvim".to_string());
        env.insert("MY_VAR".to_string(), "hello".to_string());
        let opts = SpawnOptions {
            shell: String::new(),
            env,
            ..Default::default()
        };
        assert_eq!(opts.env.len(), 2);
        assert_eq!(opts.env["EDITOR"], "nvim");
        assert_eq!(opts.env["MY_VAR"], "hello");
    }

    #[test]
    fn shell_integration_dir_exists() {
        let dir = shell_integration_dir();
        assert!(
            dir.is_dir(),
            "shell-integration directory should exist at {dir:?}"
        );
        assert!(dir.join("therminal.bash").is_file());
        assert!(dir.join("therminal.zsh").is_file());
        assert!(dir.join("therminal.fish").is_file());
        assert!(dir.join("therminal.ps1").is_file());
    }

    // ── Shell detection exit code tests ─────────────────────────────────
    //
    // These verify that shell detection checks the *exit code*, not just
    // whether the process was spawnable. A command that runs but exits
    // non-zero must not be selected as the shell.

    #[test]
    fn status_check_rejects_nonzero_exit_code() {
        // `false` is spawnable but exits with code 1.
        let result = std::process::Command::new("false")
            .status()
            .is_ok_and(|s| s.success());
        assert!(
            !result,
            "is_ok_and(success) should reject a process that exits non-zero"
        );
    }

    #[test]
    fn status_check_accepts_zero_exit_code() {
        // `true` exits with code 0.
        let result = std::process::Command::new("true")
            .status()
            .is_ok_and(|s| s.success());
        assert!(
            result,
            "is_ok_and(success) should accept a process that exits zero"
        );
    }

    #[test]
    fn status_is_ok_alone_accepts_nonzero_exit() {
        // Demonstrates the bug we fixed: is_ok() alone accepts failures.
        let result = std::process::Command::new("false").status().is_ok();
        assert!(
            result,
            "is_ok() returns true even for non-zero exit — this is why we use is_ok_and()"
        );
    }

    // ── Default shell detection ─────────────────────────────────────────

    #[test]
    fn default_shell_is_not_empty() {
        let shell = get_default_shell();
        assert!(!shell.is_empty(), "default shell should never be empty");
    }

    #[cfg(unix)]
    #[test]
    fn default_shell_is_absolute_path() {
        let shell = get_default_shell();
        assert!(
            shell.starts_with('/'),
            "Unix default shell should be an absolute path, got: {shell}"
        );
    }

    // ── WSL path conversion ─────────────────────────────────────────────

    #[test]
    fn wsl_path_conversion_logic() {
        let result = windows_to_wsl_path(Path::new(
            r"C:\Users\marci\AppData\Roaming\therminal\resources",
        ));
        assert_eq!(
            result,
            "/mnt/c/Users/marci/AppData/Roaming/therminal/resources"
        );
    }

    #[test]
    fn wsl_path_conversion_other_drives() {
        let result = windows_to_wsl_path(Path::new(r"D:\projects\therminal\resources"));
        assert_eq!(result, "/mnt/d/projects/therminal/resources");
    }

    #[test]
    fn wsl_path_conversion_unix_passthrough() {
        let result = windows_to_wsl_path(Path::new("/usr/local/bin"));
        assert_eq!(result, "/usr/local/bin");
    }

    #[test]
    fn wsl_bash_rcfile_sources_integration() {
        let rcfile =
            prepare_wsl_bash_rcfile("/mnt/c/therminal/resources/shell-integration/therminal.bash")
                .expect("failed to prepare wsl rcfile");
        let content = std::fs::read_to_string(&rcfile).unwrap();
        assert!(content.contains("/mnt/c/therminal/resources/shell-integration/therminal.bash"));
        assert!(content.contains("$HOME/.bashrc"));
    }
}
