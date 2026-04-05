# Therminal Audit

Date: 2026-04-05
Commit reviewed: `7661c93`
Primary sources: `PLAN.md`, `README.md`, beads tracker, current workspace code

## Executive Summary

The initial plan is still broadly intact: Phase 0 established the GPU terminal window, Phase 1 added semantic scrollback and passive terminal analysis, and Phase 2 delivered the daemon, IPC, split panes, config hot-reload, and control mode. The tracker now shows 49 closed issues, 28 open issues, 3 blocked issues, 25 ready issues, and 0 issues in progress.

The main mismatch is reporting, not raw implementation velocity. `PLAN.md` and beads both indicate the project has moved into Phase 3 planning, while `README.md` still says only Phase 0 and Phase 1 are complete and "Next: Phase 2." That stale status understates what has already shipped and makes the public project narrative less reliable than the tracker.

The current codebase is test-green, but several important behavioral gaps remain. The highest-risk issues are shell integration not being wired in automatically despite the docs claiming it is, and multi-pane rendering reusing pane-local caches through a single shared renderer instance.

## Planning Review

### Intended roadmap

`PLAN.md` lays out a phased build:

1. Phase 0: basic GPU terminal window
2. Phase 1: semantic scrollback and sequence interception
3. Phase 2: session daemon and multiplexing
4. Phase 3: AI detection and hotspots
5. Phase 4: MCP workspace protocol
6. Phase 5: swarm tiling and webview panes
7. Phase 6: overlay widgets and polish

### What is complete so far

Tracker and recent commits agree that Phase 0, Phase 1, and Phase 2 work have been completed:

- Phase 0 epic `tn-2er`: closed
- Phase 1 epic `tn-5n8`: closed
- Phase 2 epic `tn-pkr`: closed with all 6 child tasks complete
- Recent Phase 2 commits: daemon lifecycle, IPC, config hot-reload, session manager, split panes, control mode, and follow-up review fixes

Phase 3 and later remain open:

- Phase 3 epic `tn-91o`: open
- Phase 4 epic `tn-siz`: open, 0/2 children complete
- Phase 5 and Phase 6 epics: open

### Planning and documentation drift

- `README.md:7-20` still claims only Phase 0 and Phase 1 are complete and says "Next: Phase 2"
- beads shows Phase 2 epic `tn-pkr` closed
- `git log` contains `7661c93 Update docs: mark Phase 2 complete`, but the README status section does not reflect that state

## Code Review Findings

### High

1. Shell integration is not actually auto-installed in spawned shells

Files:
- `crates/therminal-terminal/src/pty.rs:38-49`
- `CLAUDE.md:127-135`
- `crates/therminal-terminal/tests/shell_integration.rs:14-25`

`spawn_shell()` only exports `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, and `THERMINAL_RESOURCES_DIR`. It does not source or inject the shell integration scripts. The docs claim shells will auto-source from those variables, but the test coverage only validates manual `source` in bash, not the actual PTY spawn path. On a stock shell setup, prompt boundaries and cwd markers are therefore likely missing out of the box.

2. Split-pane rendering reuses pane-local caches through a single shared `GridRenderer`

Files:
- `crates/therminal-app/src/window.rs:1121-1143`
- `crates/therminal-app/src/grid_renderer.rs:579-625`
- `crates/therminal-app/src/grid_renderer.rs:681-695`
- `crates/therminal-app/src/grid_renderer.rs:826-927`
- `crates/therminal-app/src/grid_renderer.rs:943-970`

Each pane render mutates one shared renderer by temporarily offsetting padding, but the renderer keeps stateful caches like `row_cache`, `cell_buffers`, and `last_cursor_pos`. That design is safe for a single terminal surface, not multiple panes rendered sequentially. In split layouts, stale row, cursor, or hyperlink state can bleed between panes.

### Medium

3. Control mode still hand-rolls JSON, so the "machine-readable" protocol is not trustworthy

Files:
- `crates/therminal-daemon/src/control.rs:337-429`

Most control-mode responses are built with `format!` rather than `serde_json`. `capture-pane` only escapes backslashes and quotes, not the full JSON string surface, and other response paths do not escape strings at all. Pane data, session names, or future metadata containing control characters or newlines can yield invalid JSON while still looking superficially successful on the wire.

4. Mouse routing uses the focused pane instead of the pane under the pointer

Files:
- `crates/therminal-app/src/window.rs:704-724`
- `crates/therminal-app/src/window.rs:759-824`
- `crates/therminal-app/src/window.rs:843-907`
- `crates/therminal-app/src/window.rs:967-985`

The app has `pane_at_position()`, but wheel, drag, motion, and grid coordinate translation still resolve through `focused_pane`. In a multi-pane layout that means pointer actions can target the wrong PTY until focus is explicitly changed, especially for wheel and drag paths.

5. The development `cargo run` path can point shell integration at the wrong resources directory

Files:
- `crates/therminal-runtime/src/paths.rs:103-120`
- `crates/therminal-terminal/src/pty.rs:44-47`
- `README.md:54-61`

`resources_dir()` documents a development fallback to `<workspace>/resources`, but the implementation only checks paths relative to the executable and then falls back to the data directory. Under `cargo run`, `current_exe()` points into `target/debug`, so the derived relative path becomes `target/resources`, not the repository `resources/` directory. That makes the default development workflow in the README particularly likely to miss shell integration assets.

6. Session attach claims scrollback support, but snapshots only serialize the visible screen

Files:
- `crates/therminal-daemon/src/session.rs:1-6`
- `crates/therminal-daemon/src/session.rs:185-203`
- `crates/therminal-daemon/src/session.rs:359-371`

The daemon module comment says attach/detach sends "grid + cursor + scrollback," but `Pane::snapshot()` only iterates over visible screen rows and `Session::snapshot()` just aggregates those pane snapshots. Reattached clients therefore lose terminal history that the daemon API contract implies should exist.

7. Config support is broader on paper than in practice

Files:
- `crates/therminal-core/src/config.rs:106-129`
- `crates/therminal-app/src/window.rs:910-965`
- `crates/therminal-app/src/window.rs:1124-1125`
- `crates/therminal-terminal/src/pty.rs:38-49`

The config model advertises settings for padding, shell, env, profiles, and more, but the hot-reload path currently applies only title and font changes. Rendering still hardcodes `4.0` padding, and PTY spawning ignores configured shell and env settings. That turns valid configuration into silent no-ops instead of explicit unsupported behavior.

### Additional observations

8. Fish shell integration currently emits duplicate prompt boundaries

Files:
- `resources/shell-integration/therminal.fish:30-40`
- `resources/shell-integration/therminal.fish:60-66`

The fish integration emits prompt markers from both event hooks and a wrapped `fish_prompt` function. That likely produces duplicate `OSC 133;A/B` pairs and will distort semantic command segmentation.

9. Pane replacement still allocates dummy PTYs just to swap tree nodes

Files:
- `crates/therminal-app/src/pane.rs:283-307`
- `crates/therminal-app/src/pane.rs:327-335`

`dummy_leaf()` opens a PTY solely to make `mem::replace()` work during pane tree edits. This is unnecessary resource churn in the middle of layout operations and already looks like debt rather than intentional design.

## Verification

Executed locally:

- `cargo test --workspace`
- `./scripts/ci.sh`

Result:

- Both passed
- Current automated checks do not cover the highest-impact mismatches above
- `cargo test` and `./scripts/ci.sh` both still surface one warning for an unused import in `crates/therminal-app/src/clipboard.rs:102`

## Supporting Reports

Additional focused reports were generated in:

- `audits/2026-04-05-app-core-audit.md`
- `audits/2026-04-05-daemon-runtime-top3.md`

## Recommended Next Actions

1. Fix shell integration end-to-end: correct resource discovery, inject scripts on real spawned shells, and add PTY-level tests for bash, zsh, fish, and PowerShell.
2. Make pane rendering state pane-local, or reset renderer caches between pane passes.
3. Replace all control-mode JSON assembly with `serde_json`.
4. Route pointer events by hovered pane, not focused pane.
5. Tighten config truthfulness: either implement shell/env/padding behavior or explicitly scope the supported config surface.
