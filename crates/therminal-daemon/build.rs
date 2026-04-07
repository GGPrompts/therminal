//! Build script that embeds a BUILD_HASH at compile time.
//!
//! BUILD_HASH is informational only (shown in logs and Pong responses).
//! Daemon handoff is driven by PROTOCOL_VERSION in `therminal-protocol`,
//! not by BUILD_HASH — see `ensure.rs::ensure_daemon` and the comment
//! "handoff is based on protocol version, not build hash".

use std::process::Command;

fn main() {
    // Get git short hash. Falls back to "unknown" when there is no git
    // checkout (e.g. the Windows native build, which rsyncs the repo
    // without `.git/` to keep sync fast).
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // BUILD_HASH used to embed `<git-hash>-<unix-timestamp>`, but the
    // SystemTime::now() suffix made every invocation produce a different
    // env value, so the build script was never "fresh" and the daemon
    // crate relinked on every cargo build. Since BUILD_HASH is purely
    // informational, embedding just the git hash is enough.
    println!("cargo:rustc-env=BUILD_HASH={git_hash}");

    // NOTE: deliberately do NOT declare `rerun-if-changed=../../.git/HEAD`.
    // When the build runs from a checkout that lacks `.git/` (the rsync'd
    // Windows build dir), cargo treats the missing path as stale on every
    // invocation and reruns the build script, which forces the daemon
    // binary to relink. `rerun-if-changed=src/` is sufficient: if you
    // commit without touching code, the embedded hash is slightly stale
    // until the next source edit, which is fine for an informational
    // field.
    println!("cargo:rerun-if-changed=src/");
}
