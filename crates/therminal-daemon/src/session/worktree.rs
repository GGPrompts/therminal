//! Git worktree resolution + creation for `IpcRequest::SplitPane { worktree, .. }`
//! (tn-h7tq).
//!
//! When a pane create request carries an explicit `worktree = "<branch>"`,
//! the daemon needs to:
//!
//! 1. Resolve the source pane's git repo root.
//! 2. Check whether a worktree for that branch already exists. If yes,
//!    reuse its path so two delegates pointed at the same branch land in
//!    the same on-disk tree.
//! 3. Otherwise create one with `git worktree add <repo>/../<repo>-<branch> <branch>`.
//! 4. Return the resolved path so the caller can spawn the pane cd'd
//!    there and tag it with `branch=` / `worktree=` / `repo=`.
//!
//! All git interaction is shell-out — we deliberately avoid pulling in a
//! git crate to keep the daemon dependency surface small. The functions
//! return `Result<_, String>` so the daemon's IPC dispatch loop can
//! forward error messages directly to the client.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Outcome of a worktree resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorktree {
    /// Absolute path to the worktree's working directory.
    pub path: PathBuf,
    /// Repository basename (e.g. `therminal` for `~/projects/therminal`).
    pub repo_name: String,
    /// Branch the worktree is checked out on.
    pub branch: String,
    /// Whether this call created the worktree (`true`) or reused an
    /// existing one (`false`).
    pub created: bool,
}

/// Resolve the git repo root that contains `cwd` by shelling out to
/// `git -C <cwd> rev-parse --show-toplevel`. Returns the absolute path on
/// success or a human-readable error otherwise.
pub fn resolve_repo_root(cwd: &str) -> Result<PathBuf, String> {
    if cwd.is_empty() {
        return Err("source pane has no cwd — cannot resolve git repo".into());
    }
    let cwd_path = Path::new(cwd);
    if !cwd_path.exists() {
        return Err(format!("source pane cwd does not exist: {cwd}"));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd_path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("failed to invoke git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git rev-parse --show-toplevel failed in {cwd}: {}",
            stderr.trim()
        ));
    }
    let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path_str.is_empty() {
        return Err(format!("git returned an empty repo root for {cwd}"));
    }
    Ok(PathBuf::from(path_str))
}

/// Resolve `<branch>` to a worktree path under `repo_root`. If a worktree
/// already exists for that branch (anywhere on disk), its path is returned
/// with `created = false`. Otherwise the function creates one at
/// `<repo_root parent>/<repo_name>-<sanitised-branch>` and returns
/// `created = true`.
///
/// `branch` must already exist in the repo — the function does not create
/// branches. If you need new-branch behaviour, layer it on top of this
/// primitive.
pub fn find_or_create_worktree(repo_root: &Path, branch: &str) -> Result<ResolvedWorktree, String> {
    if branch.trim().is_empty() {
        return Err("worktree branch name must not be empty".into());
    }
    let repo_name = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            format!(
                "could not derive repo basename from {}",
                repo_root.display()
            )
        })?
        .to_string();

    // 1. Check for an existing worktree on this branch.
    if let Some(existing) = find_existing_worktree(repo_root, branch)? {
        return Ok(ResolvedWorktree {
            path: existing,
            repo_name,
            branch: branch.to_string(),
            created: false,
        });
    }

    // 2. Verify the branch exists before we try `worktree add`. We give a
    //    clearer error message than git's default "invalid reference".
    if !branch_exists(repo_root, branch)? {
        return Err(format!(
            "branch {branch} does not exist in {} — create it first (git branch {branch})",
            repo_root.display()
        ));
    }

    // 3. Compute the target path: <parent>/<repo>-<sanitised>
    let parent = repo_root
        .parent()
        .ok_or_else(|| format!("repo root has no parent directory: {}", repo_root.display()))?;
    let sanitised = sanitise_branch_for_path(branch);
    let target = parent.join(format!("{repo_name}-{sanitised}"));

    if target.exists() {
        return Err(format!(
            "worktree target path already exists but is not a registered worktree: {}",
            target.display()
        ));
    }

    // 4. Create it.
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "add"])
        .arg(&target)
        .arg(branch)
        .output()
        .map_err(|e| format!("failed to invoke git worktree add: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git worktree add {} {branch} failed: {}",
            target.display(),
            stderr.trim()
        ));
    }

    Ok(ResolvedWorktree {
        path: target,
        repo_name,
        branch: branch.to_string(),
        created: true,
    })
}

/// Parse `git worktree list --porcelain` output and return the path of the
/// worktree currently checked out on `branch`, if any.
fn find_existing_worktree(repo_root: &Path, branch: &str) -> Result<Option<PathBuf>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("failed to invoke git worktree list: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree list failed: {}", stderr.trim()));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(parse_worktree_list_for_branch(&text, branch))
}

/// Pure-string parser broken out for unit testing. The porcelain format is
/// blocks separated by blank lines:
///
/// ```text
/// worktree /abs/path
/// HEAD <sha>
/// branch refs/heads/<name>
///
/// worktree /abs/path-2
/// HEAD <sha>
/// branch refs/heads/other
/// ```
///
/// Detached worktrees emit `detached` instead of `branch <ref>`.
pub(crate) fn parse_worktree_list_for_branch(text: &str, branch: &str) -> Option<PathBuf> {
    let target_ref = format!("refs/heads/{branch}");
    let mut current_path: Option<PathBuf> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path.trim()));
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            if branch_ref.trim() == target_ref
                && let Some(p) = current_path.take()
            {
                return Some(p);
            }
        } else if line.trim().is_empty() {
            current_path = None;
        }
    }
    None
}

/// Return true if `branch` exists as a local ref in the repo.
fn branch_exists(repo_root: &Path, branch: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .map_err(|e| format!("failed to invoke git show-ref: {e}"))?;
    Ok(output.status.success())
}

/// Replace path-unsafe characters in a branch name so the resulting
/// directory name (`<repo>-<sanitised>`) is portable across filesystems.
/// Slashes (e.g. `feat/foo`) become dashes; everything else passes through.
fn sanitise_branch_for_path(branch: &str) -> String {
    branch.replace(['/', '\\', ':'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_worktree_list_finds_branch() {
        let text = "\
worktree /home/u/projects/therminal
HEAD abc123
branch refs/heads/main

worktree /home/u/projects/therminal-feat-x
HEAD def456
branch refs/heads/feat-x

";
        let p = parse_worktree_list_for_branch(text, "feat-x");
        assert_eq!(p, Some(PathBuf::from("/home/u/projects/therminal-feat-x")));
        assert_eq!(
            parse_worktree_list_for_branch(text, "main"),
            Some(PathBuf::from("/home/u/projects/therminal"))
        );
        assert_eq!(parse_worktree_list_for_branch(text, "missing"), None);
    }

    #[test]
    fn parse_worktree_list_handles_detached() {
        let text = "\
worktree /home/u/projects/therminal
HEAD abc123
branch refs/heads/main

worktree /home/u/projects/scratch
HEAD def456
detached

";
        // Detached entry should not match any branch lookup.
        assert_eq!(
            parse_worktree_list_for_branch(text, "main"),
            Some(PathBuf::from("/home/u/projects/therminal"))
        );
        assert_eq!(parse_worktree_list_for_branch(text, "scratch"), None);
    }

    #[test]
    fn sanitise_branch_replaces_slashes() {
        assert_eq!(sanitise_branch_for_path("feat/foo"), "feat-foo");
        assert_eq!(sanitise_branch_for_path("ggp/feat/bar"), "ggp-feat-bar");
        assert_eq!(sanitise_branch_for_path("plain"), "plain");
        assert_eq!(sanitise_branch_for_path("a:b\\c"), "a-b-c");
    }

    #[test]
    fn resolve_repo_root_rejects_empty_cwd() {
        let err = resolve_repo_root("").unwrap_err();
        assert!(err.contains("no cwd"));
    }

    /// End-to-end test against a real temp git repo. Skipped on systems
    /// where `git` isn't on PATH (which would also break the daemon's
    /// shell-out path so the regression isn't relevant).
    #[test]
    fn find_or_create_worktree_round_trip() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("toy");
        std::fs::create_dir_all(&repo).unwrap();

        run_git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README"), "hi\n").unwrap();
        run_git(&repo, &["add", "README"]);
        run_git(&repo, &["commit", "--quiet", "-m", "init"]);
        run_git(&repo, &["branch", "feature-x"]);

        let resolved = find_or_create_worktree(&repo, "feature-x").expect("create worktree");
        assert!(resolved.created, "first call should create");
        assert_eq!(resolved.repo_name, "toy");
        assert_eq!(resolved.branch, "feature-x");
        assert!(resolved.path.exists(), "worktree path should exist");
        assert_eq!(
            resolved.path,
            tmp.path().join("toy-feature-x"),
            "worktree path should be <parent>/<repo>-<branch>"
        );

        // Second call must reuse, not re-create.
        let again = find_or_create_worktree(&repo, "feature-x").expect("reuse");
        assert!(!again.created, "second call should reuse");
        assert_eq!(again.path, resolved.path);
    }

    #[test]
    fn find_or_create_worktree_rejects_missing_branch() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("toy");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        run_git(&repo, &["config", "user.email", "test@example.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README"), "hi\n").unwrap();
        run_git(&repo, &["add", "README"]);
        run_git(&repo, &["commit", "--quiet", "-m", "init"]);

        let err = find_or_create_worktree(&repo, "no-such-branch").unwrap_err();
        assert!(
            err.contains("does not exist"),
            "error should mention missing branch, got: {err}"
        );
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed");
    }
}
