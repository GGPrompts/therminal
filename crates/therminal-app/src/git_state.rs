//! Git repository state detection by reading `.git/HEAD` directly.
//!
//! No shelling out to `git` -- all detection is done by reading files from the
//! filesystem, which keeps latency low and avoids spawning processes on every
//! cwd change.

use std::path::{Path, PathBuf};

/// Snapshot of the git state for a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitState {
    /// Branch name (e.g. "main", "feat/foo") or short commit hash when detached.
    pub branch: String,
    /// Whether the working tree has uncommitted changes (index differs from HEAD).
    pub dirty: bool,
    /// Whether this is a git worktree (`.git` is a file, not a directory).
    pub is_worktree: bool,
    /// Whether HEAD is detached (not pointing at a branch ref).
    pub detached: bool,
}

/// Detect git state for the given working directory.
///
/// Walks up from `cwd` to find a `.git` entry, reads `HEAD` to determine
/// the current branch (or detached commit), and stats the index file to
/// approximate dirtiness.
///
/// Returns `None` if `cwd` is not inside a git repository.
pub fn detect(cwd: &Path) -> Option<GitState> {
    let (git_dir, is_worktree) = find_git_dir(cwd)?;

    let head_contents = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head_contents = head_contents.trim();

    let (branch, detached) = parse_head(head_contents);
    let dirty = check_dirty(&git_dir);

    Some(GitState {
        branch,
        dirty,
        is_worktree,
        detached,
    })
}

/// Walk up from `start` looking for a `.git` entry (directory or file).
///
/// Returns `(git_dir_path, is_worktree)` where `git_dir_path` is the actual
/// `.git` directory (resolved through worktree indirection if needed).
fn find_git_dir(start: &Path) -> Option<(PathBuf, bool)> {
    let mut current = start.to_path_buf();
    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() {
            return Some((dot_git, false));
        }
        if dot_git.is_file() {
            // Worktree: `.git` is a file containing `gitdir: <path>`
            if let Some(resolved) = resolve_gitdir_file(&dot_git) {
                return Some((resolved, true));
            }
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Parse a `.git` file (worktree pointer) to extract the real gitdir path.
///
/// Format: `gitdir: /path/to/repo/.git/worktrees/<name>`
fn resolve_gitdir_file(dot_git_file: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(dot_git_file).ok()?;
    let line = contents.trim();
    let path_str = line.strip_prefix("gitdir: ")?;
    let path = PathBuf::from(path_str);
    if path.is_absolute() {
        if path.is_dir() { Some(path) } else { None }
    } else {
        // Relative to the directory containing the `.git` file.
        let base = dot_git_file.parent()?;
        let resolved = base.join(path_str);
        let canonical = resolved.canonicalize().ok()?;
        if canonical.is_dir() {
            Some(canonical)
        } else {
            None
        }
    }
}

/// Parse the contents of `HEAD` to extract a branch name or short hash.
///
/// Returns `(name, is_detached)`.
fn parse_head(head: &str) -> (String, bool) {
    // Normal branch: "ref: refs/heads/main"
    if let Some(ref_path) = head.strip_prefix("ref: ") {
        let branch = ref_path
            .strip_prefix("refs/heads/")
            .unwrap_or(ref_path)
            .to_string();
        (branch, false)
    } else {
        // Detached HEAD: raw commit hash. Show first 7 chars.
        let short = if head.len() >= 7 { &head[..7] } else { head };
        (short.to_string(), true)
    }
}

/// Approximate dirty check: compare index mtime to HEAD commit mtime.
///
/// This is a heuristic -- we check if the index file exists and was modified
/// more recently than the HEAD ref target. For speed, we only stat files
/// rather than parsing git objects.
fn check_dirty(git_dir: &Path) -> bool {
    let index_path = git_dir.join("index");

    // If there's no index file, this might be a fresh repo with no commits.
    let index_meta = match std::fs::metadata(&index_path) {
        Ok(m) => m,
        Err(_) => return false,
    };

    // Read HEAD to find the ref file.
    let head_path = git_dir.join("HEAD");
    let head_contents = match std::fs::read_to_string(&head_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let head_trimmed = head_contents.trim();

    // Find the ref file that HEAD points to.
    let ref_path = if let Some(ref_name) = head_trimmed.strip_prefix("ref: ") {
        git_dir.join(ref_name)
    } else {
        // Detached HEAD -- no ref file to compare against.
        // Fall back to comparing index mtime vs HEAD file mtime.
        head_path
    };

    let ref_meta = match std::fs::metadata(&ref_path) {
        Ok(m) => m,
        Err(_) => {
            // Ref file doesn't exist -- possibly a fresh repo with a branch
            // that has no commits yet. If the index exists, it's "dirty".
            return true;
        }
    };

    // If the index was modified after the ref, there are likely staged changes.
    let index_modified = index_meta.modified().ok();
    let ref_modified = ref_meta.modified().ok();
    match (index_modified, ref_modified) {
        (Some(idx), Some(rf)) => idx > rf,
        _ => false,
    }
}

/// Format git state for display in the pane header.
///
/// Returns a string like "main", "feat/foo *", or "abc1234".
pub fn format_for_header(state: &GitState) -> String {
    let dirty_marker = if state.dirty { " *" } else { "" };
    format!("{}{dirty_marker}", state.branch)
}

/// Format git state for display in the status bar.
///
/// Returns a string like "(main)", "(feat/foo *)", or "(abc1234 detached)".
pub fn format_for_status_bar(state: &GitState) -> String {
    let dirty_marker = if state.dirty { " *" } else { "" };
    if state.detached {
        format!("({}{dirty_marker} detached)", state.branch)
    } else {
        format!("({}{dirty_marker})", state.branch)
    }
}

/// Returns true if the branch is a default/mainline branch (main, master, develop).
pub fn is_default_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "develop" | "dev" | "trunk")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_head_branch() {
        let (name, detached) = parse_head("ref: refs/heads/main");
        assert_eq!(name, "main");
        assert!(!detached);
    }

    #[test]
    fn parse_head_feature_branch() {
        let (name, detached) = parse_head("ref: refs/heads/feat/login-page");
        assert_eq!(name, "feat/login-page");
        assert!(!detached);
    }

    #[test]
    fn parse_head_detached() {
        let (name, detached) = parse_head("abc1234def5678901234567890abcdef12345678");
        assert_eq!(name, "abc1234");
        assert!(detached);
    }

    #[test]
    fn parse_head_short_hash() {
        let (name, detached) = parse_head("abc");
        assert_eq!(name, "abc");
        assert!(detached);
    }

    #[test]
    fn format_header_clean() {
        let state = GitState {
            branch: "main".into(),
            dirty: false,
            is_worktree: false,
            detached: false,
        };
        assert_eq!(format_for_header(&state), "main");
    }

    #[test]
    fn format_header_dirty() {
        let state = GitState {
            branch: "feat/foo".into(),
            dirty: true,
            is_worktree: false,
            detached: false,
        };
        assert_eq!(format_for_header(&state), "feat/foo *");
    }

    #[test]
    fn format_header_detached() {
        let state = GitState {
            branch: "abc1234".into(),
            dirty: false,
            is_worktree: false,
            detached: true,
        };
        assert_eq!(format_for_header(&state), "abc1234");
    }

    #[test]
    fn format_status_bar_clean() {
        let state = GitState {
            branch: "main".into(),
            dirty: false,
            is_worktree: false,
            detached: false,
        };
        assert_eq!(format_for_status_bar(&state), "(main)");
    }

    #[test]
    fn format_status_bar_dirty() {
        let state = GitState {
            branch: "feat/bar".into(),
            dirty: true,
            is_worktree: false,
            detached: false,
        };
        assert_eq!(format_for_status_bar(&state), "(feat/bar *)");
    }

    #[test]
    fn format_status_bar_detached_dirty() {
        let state = GitState {
            branch: "abc1234".into(),
            dirty: true,
            is_worktree: false,
            detached: true,
        };
        assert_eq!(format_for_status_bar(&state), "(abc1234 * detached)");
    }

    #[test]
    fn is_default_branch_known() {
        assert!(is_default_branch("main"));
        assert!(is_default_branch("master"));
        assert!(is_default_branch("develop"));
        assert!(is_default_branch("dev"));
        assert!(is_default_branch("trunk"));
    }

    #[test]
    fn is_default_branch_feature() {
        assert!(!is_default_branch("feat/foo"));
        assert!(!is_default_branch("fix/bar"));
        assert!(!is_default_branch("release/1.0"));
    }

    #[test]
    fn detect_current_repo() {
        // This test assumes we're running inside a git repo.
        let cwd = std::env::current_dir().unwrap();
        let state = detect(&cwd);
        // If CI or the test runner is inside a git repo, we should get Some.
        // If not (e.g. running from a tarball), the test still passes.
        if cwd.join(".git").exists() || find_git_dir(&cwd).is_some() {
            assert!(state.is_some(), "expected git state for {}", cwd.display());
            let s = state.unwrap();
            assert!(!s.branch.is_empty());
        }
    }

    #[test]
    fn detect_non_repo() {
        // /tmp is unlikely to be a git repo.
        let state = detect(Path::new("/tmp"));
        // This might be Some if /tmp is inside a git worktree somehow,
        // but on most systems it won't be.
        if !Path::new("/tmp/.git").exists() {
            assert!(state.is_none());
        }
    }
}
