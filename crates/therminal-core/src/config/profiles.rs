//! Profile resolution: bridges [`ProfileConfig`] to PTY spawn parameters.
//!
//! The resolver produces a [`ResolvedProfile`] that carries the same fields as
//! `therminal_terminal::pty::SpawnOptions` plus `skip_shell_integration`.
//! Downstream crates that depend on both `therminal-core` and
//! `therminal-terminal` convert via `From<ResolvedProfile> for SpawnOptions`.

use std::collections::HashMap;

use thiserror::Error;
use tracing::warn;

use super::ProfileConfig;

/// Error returned by [`resolve_profile`].
#[derive(Debug, Error)]
pub enum ProfileResolveError {
    /// The requested profile name does not exist in the config.
    #[error("unknown profile: {0:?}")]
    NotFound(String),
}

/// Resolved spawn parameters produced from a [`ProfileConfig`].
///
/// This struct mirrors the fields needed by
/// `therminal_terminal::pty::SpawnOptions` so that the conversion is trivial
/// (a `From` impl in `therminal-terminal`).
#[derive(Debug, Clone)]
pub struct ResolvedProfile {
    /// Shell binary or command string. Empty = use system default.
    pub shell: String,
    /// Extra arguments passed to the shell/command.
    pub shell_args: Vec<String>,
    /// Extra environment variables to merge into the PTY environment.
    pub env: HashMap<String, String>,
    /// Working directory. Empty = inherit from caller.
    pub cwd: String,
    /// Whether to skip shell-integration injection (rcfile wrappers, ZDOTDIR,
    /// etc.).  `true` for `command`-mode profiles unless the profile
    /// explicitly opts in via `shell_integration = true`.
    pub skip_shell_integration: bool,
}

/// Resolve a named profile from the config map into PTY spawn parameters.
///
/// # Resolution rules
///
/// 1. **`command` wins over `shell`**: if the profile sets `command`, the
///    command string becomes `shell` and `shell_args` is cleared (the
///    command is a self-contained invocation).  If both `command` and
///    `shell` are set, a warning is emitted and `command` wins.
///
/// 2. **`shell_integration`**: auto-derived from the launch mode.
///    - `command` mode: `skip_shell_integration = true` (commands like
///      `docker exec` or `ssh` are unlikely to benefit).
///    - `shell` mode: `skip_shell_integration = false`.
///    - An explicit `shell_integration` field on the profile overrides the
///      auto-derived value in either direction.
///
/// 3. **Working directory**: `profile.working_directory` takes precedence;
///    if absent, `inherit_cwd` is used.
///
/// 4. **Environment**: `profile.env` entries are merged verbatim.
pub fn resolve_profile(
    profiles: &HashMap<String, ProfileConfig>,
    name: &str,
    inherit_cwd: &str,
) -> Result<ResolvedProfile, ProfileResolveError> {
    let profile = profiles
        .get(name)
        .ok_or_else(|| ProfileResolveError::NotFound(name.to_owned()))?;

    // Determine shell/command and auto-derive skip_shell_integration.
    let (shell, shell_args, auto_skip) = if let Some(ref command) = profile.command {
        if profile.shell.is_some() {
            warn!(
                profile = name,
                "profile has both `command` and `shell`; `command` takes precedence"
            );
        }
        // Command mode: the command string is the binary, no extra shell_args
        // from the profile (shell_args are only meaningful for shell mode).
        (command.clone(), Vec::new(), true)
    } else {
        // Shell mode: use shell + shell_args (or empty = system default).
        let shell = profile.shell.clone().unwrap_or_default();
        (shell, profile.shell_args.clone(), false)
    };

    // Explicit override beats auto-derived value.
    let skip_shell_integration = match profile.shell_integration {
        Some(explicit) => !explicit, // shell_integration=true => skip=false
        None => auto_skip,
    };

    let cwd = profile
        .working_directory
        .clone()
        .unwrap_or_else(|| inherit_cwd.to_owned());

    Ok(ResolvedProfile {
        shell,
        shell_args,
        env: profile.env.clone(),
        cwd,
        skip_shell_integration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_profiles(pairs: Vec<(&str, ProfileConfig)>) -> HashMap<String, ProfileConfig> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
    }

    #[test]
    fn not_found_returns_error() {
        let profiles = HashMap::new();
        let err = resolve_profile(&profiles, "nope", "/tmp").unwrap_err();
        assert!(
            matches!(err, ProfileResolveError::NotFound(ref n) if n == "nope"),
            "expected NotFound, got: {err:?}"
        );
    }

    #[test]
    fn shell_only_profile() {
        let profiles = make_profiles(vec![(
            "dev",
            ProfileConfig {
                shell: Some("/usr/bin/fish".into()),
                shell_args: vec!["--private".into()],
                ..Default::default()
            },
        )]);
        let resolved = resolve_profile(&profiles, "dev", "/home/user").unwrap();
        assert_eq!(resolved.shell, "/usr/bin/fish");
        assert_eq!(resolved.shell_args, vec!["--private"]);
        assert!(!resolved.skip_shell_integration, "shell mode should inject integration");
        assert_eq!(resolved.cwd, "/home/user");
    }

    #[test]
    fn command_only_profile() {
        let profiles = make_profiles(vec![(
            "docker",
            ProfileConfig {
                command: Some("docker exec -it mycontainer bash".into()),
                ..Default::default()
            },
        )]);
        let resolved = resolve_profile(&profiles, "docker", "/tmp").unwrap();
        assert_eq!(resolved.shell, "docker exec -it mycontainer bash");
        assert!(resolved.shell_args.is_empty(), "command mode ignores shell_args");
        assert!(resolved.skip_shell_integration, "command mode should skip integration");
    }

    #[test]
    fn command_wins_over_shell() {
        let profiles = make_profiles(vec![(
            "both",
            ProfileConfig {
                shell: Some("/bin/zsh".into()),
                shell_args: vec!["--login".into()],
                command: Some("ssh remote".into()),
                ..Default::default()
            },
        )]);
        let resolved = resolve_profile(&profiles, "both", "/tmp").unwrap();
        assert_eq!(resolved.shell, "ssh remote", "command should win");
        assert!(resolved.shell_args.is_empty(), "shell_args cleared in command mode");
        assert!(resolved.skip_shell_integration);
    }

    #[test]
    fn explicit_shell_integration_override_in_command_mode() {
        let profiles = make_profiles(vec![(
            "custom",
            ProfileConfig {
                command: Some("bash -l".into()),
                shell_integration: Some(true),
                ..Default::default()
            },
        )]);
        let resolved = resolve_profile(&profiles, "custom", "/tmp").unwrap();
        assert!(!resolved.skip_shell_integration, "explicit true should override command-mode auto-skip");
    }

    #[test]
    fn explicit_shell_integration_override_in_shell_mode() {
        let profiles = make_profiles(vec![(
            "raw",
            ProfileConfig {
                shell: Some("/bin/bash".into()),
                shell_integration: Some(false),
                ..Default::default()
            },
        )]);
        let resolved = resolve_profile(&profiles, "raw", "/tmp").unwrap();
        assert!(resolved.skip_shell_integration, "explicit false should disable integration in shell mode");
    }

    #[test]
    fn working_directory_overrides_inherit_cwd() {
        let profiles = make_profiles(vec![(
            "proj",
            ProfileConfig {
                working_directory: Some("/projects/foo".into()),
                ..Default::default()
            },
        )]);
        let resolved = resolve_profile(&profiles, "proj", "/home/user").unwrap();
        assert_eq!(resolved.cwd, "/projects/foo");
    }

    #[test]
    fn inherit_cwd_when_no_working_directory() {
        let profiles = make_profiles(vec![("bare", ProfileConfig::default())]);
        let resolved = resolve_profile(&profiles, "bare", "/home/user").unwrap();
        assert_eq!(resolved.cwd, "/home/user");
    }

    #[test]
    fn env_merge() {
        let mut env = HashMap::new();
        env.insert("RUST_LOG".into(), "debug".into());
        env.insert("MY_VAR".into(), "hello".into());
        let profiles = make_profiles(vec![(
            "envtest",
            ProfileConfig {
                env,
                ..Default::default()
            },
        )]);
        let resolved = resolve_profile(&profiles, "envtest", "/tmp").unwrap();
        assert_eq!(resolved.env.len(), 2);
        assert_eq!(resolved.env.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(resolved.env.get("MY_VAR").map(String::as_str), Some("hello"));
    }

    #[test]
    fn default_profile_uses_system_defaults() {
        // A profile with all defaults should produce empty shell (= system default),
        // no args, no env, inherited cwd, and integration enabled.
        let profiles = make_profiles(vec![("default", ProfileConfig::default())]);
        let resolved = resolve_profile(&profiles, "default", "/home/user").unwrap();
        assert!(resolved.shell.is_empty());
        assert!(resolved.shell_args.is_empty());
        assert!(resolved.env.is_empty());
        assert_eq!(resolved.cwd, "/home/user");
        assert!(!resolved.skip_shell_integration);
    }
}
