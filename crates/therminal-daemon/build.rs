//! Build script that embeds a BUILD_HASH at compile time.
//!
//! The hash is `<git-short-hash>-<unix-timestamp>`, providing a unique
//! identifier for each build. This is used for version-mismatch detection
//! during daemon handoff — if a new binary has a different BUILD_HASH than
//! the running daemon, a graceful handoff is triggered.

use std::process::Command;

fn main() {
    // Get git short hash
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Get Unix timestamp
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());

    let build_hash = format!("{git_hash}-{timestamp}");
    println!("cargo:rustc-env=BUILD_HASH={build_hash}");

    // Rebuild when git HEAD changes or source changes
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/");
    println!("cargo:rerun-if-changed=src/");
}
