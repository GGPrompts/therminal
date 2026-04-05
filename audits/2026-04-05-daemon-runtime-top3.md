# Daemon/Runtime Audit: Top 3 Findings

Date: 2026-04-05
Scope: `crates/therminal-daemon`, `crates/therminal-runtime`

## 1. Development builds point shells at a non-existent resources directory

Files:
- `crates/therminal-runtime/src/paths.rs:103`
- `crates/therminal-terminal/src/pty.rs:40`
- `README.md:54`

`resources_dir()` documents support for resolving `<workspace>/resources` during development, but the implementation only checks paths relative to the compiled executable and then falls back to `<data_dir>/resources`. In a normal `cargo run --bin therminal` flow, `current_exe()` resolves under `target/debug`, so `../resources` becomes `target/resources`, not the repository’s `resources/` directory. `spawn_shell()` then exports that wrong path via `THERMINAL_RESOURCES_DIR`, which means shell integration is likely broken in the default dev path described in the README. This is a concrete runtime mismatch: one of the advertised development workflows can silently lose prompt markers and cwd reporting because the scripts are never found.

## 2. Control mode still hand-rolls JSON and can emit invalid protocol payloads

Files:
- `crates/therminal-daemon/src/control.rs:337`
- `crates/therminal-daemon/src/control.rs:354`
- `crates/therminal-daemon/src/control.rs:378`

The control protocol claims machine-readable output, but most responses are assembled with `format!` rather than `serde_json`. That leaves correctness dependent on ad hoc escaping. The current `capture-pane` path only escapes backslashes and quotes inside lines, and the other JSON responses do not escape string fields at all. Any future pane/session metadata containing control characters, embedded newlines, or other JSON-sensitive content will produce invalid payloads even though the transport framing is correct. This is both a bug and a protocol design mismatch: the daemon exposes a structured control surface, but its encoding is not actually trustworthy as a general machine interface.

## 3. Session attach promises scrollback, but snapshots only include the visible screen

Files:
- `crates/therminal-daemon/src/session.rs:1`
- `crates/therminal-daemon/src/session.rs:185`
- `crates/therminal-daemon/src/session.rs:359`

The module-level contract says attach/detach sends “grid + cursor + scrollback,” but `Pane::snapshot()` only iterates `0..screen_lines()` and serializes the visible grid. Nothing from scrollback history is captured, and `Session::snapshot()` simply aggregates those truncated pane snapshots. That means clients reattaching to a daemon-managed session cannot reconstruct the prior terminal context the comments promise, which is a meaningful behavior gap for a multiplexed terminal. Given that semantic history and attach/detach are central to the daemon design, this mismatch is likely to surface as user-visible data loss rather than a minor documentation issue.

## Validation

I validated the current tree with:

- `cargo test --workspace`
- `./scripts/ci.sh`

Both passed. The issues above are design/runtime mismatches that the current automated coverage does not catch.
