# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

**Phases 0–5 complete; Phase 6 (overlay widgets) in progress. The GUI is a daemon client by default (tn-382v Phase B / tn-beez): split / close / focus route through `IpcRequest::SplitPane` / `KillPane` / `SelectPane`, so MCP `terminal.panes.*` operations act on the visible window and layouts persist across daemon restarts. The GUI auto-spawns `therminal-daemon` on startup if no daemon is running (tn-txs8), with `[daemon] binary_path` as a config override. `mcp.attach_mode = "local"` remains as a one-release escape hatch.** The terminal renders with wgpu, runs a shell, handles keyboard + mouse input, has a SequenceInterceptor for AI-aware OSC parsing, semantic region indexing, shell integration scripts, process tree agent detection with a live agent registry tracking status across panes, output cadence analysis, persistent multiplexed sessions via daemon with socket-as-lock, zero-downtime handoff with SCM_RIGHTS FD passing, debounced session state persistence, tn-zamd structured `PaneStateSnapshot` replay on reattach (mode flags / cursor / visible grid synthesized back into the GUI's local `Term` so TUI mouse capture + cursor visibility survive GUI restarts), IPC protocol (MessagePack framing), split panes with mouse-drag separator resize, auto-tiling for agent swarms, PaneBackend abstraction (Terminal | WebView), workspace tabs with inline rename, pane swap, font size keybindings, keybinding help overlay (auto-sizing with two-column fallback), right-click context menus (including Copy pane ID), click-to-open hotspots (file paths, URLs, errors) with wrapped-line joining, pointer cursor affordance, configurable editor fallback chain and toast feedback on failures, directory hotspots routed through `folder_pane_command` (default `tfe`) for in-pane spawn and `folder_opener` chain for "reveal in file manager" (tn-zqwg), optional `show_pane_headers` toggle with focused-pane info in the footer, cwd inheritance on pane split, TOML config with hot-reload, JSONL event logging, control mode, MCP Resources with subscription-based pane content streaming, live Claude Code session observability via `/tmp/claude-code-state/` watcher + `~/.claude/projects/` JSONL tailers (parent + subagents, `TaggedAgentEvent` streamed over `therminal://claude/events`, plus a `claude-events` CLI viewer dev tool), agent lifecycle events streamed over `therminal://agents/events`, OSC 633 command transcript exposed via `terminal.semantic.query_commands`, window edge resize + cursor affordances under CSD mode, and opaque per-pane key/value tag bag (tn-bbvf) for binding panes to external concepts (issue ids, branches, worker ids) — set via `terminal.panes.tag` / `terminal.panes.untag`, surfaced in `terminal.panes.list`, and persisted across daemon restarts, an MCP server exposing 30 tools across 7 domains with trust tier enforcement, a shipped semantic pattern matching engine (tn-yrjd) wired into PTY line finalization and OSC 133 C (tn-86us) with live `emit_event` dispatch and authoring via TOML pattern packs (see `docs/pattern-matching-spec.md` and the bundled `therminal-plugin` Claude Code skill under `resources/skills/`), harness-resolved Claude Code tool-call hotspots that resolve relative paths against the agent's cwd across worktree hops (tn-gidy), delegate profile schema under `[delegate.profiles.*]` for planned sibling-Claude spawning (tn-ztv3.1, schema only), delegate result capture via `terminal.panes.capture_result` using transcript-first with grid fallback (tn-ztv3.5), and a cache-friendly `therminal pane|session|workspace|agents|events|semantic` CLI surface (tn-k13n) that wraps the daemon-client for MCP consumers (Claude Code, Codex, scripts) to drive the same daemon without paying MCP framing costs — terse TSV by default (`pane list` for 5 panes is ≤ 150 bytes), `--json` for structured callers, see `docs/cli.md`.

## Architecture

Cargo workspace with seven crates. Each crate has its own `CLAUDE.md` with deep architecture docs.

```
crates/
├── therminal-protocol/       # Wire types, MCP schema, semantic events
├── therminal-terminal/       # PTY, OSC parsing, state inference, agent detection, region index
├── therminal-core/           # Color palette, wgpu context, text renderer, TOML config, hot-reload
├── therminal-runtime/        # Cross-platform paths, runtime dir management
├── therminal-daemon/         # Session manager, event bus, multiplexer, MCP server, trust enforcement
├── therminal-harness-claude/ # First-class Claude Code integration (JSONL tailer + state watcher + event stream)
└── therminal-app/            # winit window, grid renderer, mouse input, PTY wiring
vendor/
├── alacritty_terminal/    # Vendored v0.25.1
└── vte/                   # Vendored with SequenceInterceptor trait
resources/
└── shell-integration/     # bash, zsh, fish, PowerShell scripts
plugins/                   # Pattern packs (TOML config, see plugins/CLAUDE.md)
└── examples/              # Shipped example packs loaded by default
```

### Core Stack
- **GPU rendering**: wgpu + glyphon + cosmic-text
- **Terminal emulation**: alacritty_terminal (vendored) + VTE with SequenceInterceptor
- **Windowing**: winit (cross-platform)
- **PTY**: portable-pty (cross-platform), shared lifecycle via `PtyPaneCore` in therminal-terminal
- **Configuration**: toml + serde (TOML config), notify (file watcher for hot-reload)
- **Agent detection**: sysinfo (process tree), cadence analysis (output stream timing)
- **IPC**: interprocess crate (Unix sockets / named pipes)
- **Wire protocol**: MessagePack framing
- **Language**: Rust

## Shell Integration

Therminal uses Ghostty-style `TERM_PROGRAM` detection. When spawning a PTY, three env vars are set:

- `TERM_PROGRAM=therminal` -- shells use this to detect the terminal and auto-source integration scripts
- `TERM_PROGRAM_VERSION` -- the crate version from `Cargo.toml`
- `THERMINAL_RESOURCES_DIR` -- absolute path to the resources directory containing shell scripts

Shell integration scripts live in `resources/shell-integration/` (bash, zsh, fish, PowerShell). Each script emits OSC 133 marks (A=PromptStart, B=PromptEnd, C=PreExec, D=CommandFinished) and OSC 7 for current directory. All scripts guard against double-sourcing via `__THERMINAL_SHELL_INTEGRATION`.

## WSL2

Therminal runs fine on WSL2. Key quirks:

- **Windows home abbreviation**: `abbreviate_path()` in `chrome.rs` detects WSL2 via `WSL_DISTRO_NAME` and abbreviates `/mnt/c/Users/<user>` to `~win` using `USERPROFILE` env var.
- **Process detector false positives**: `is_wsl2_interop_process()` in `process_detector.rs` filters out Windows `.exe` files launched via binfmt_misc interop before agent classification.
- **GPU**: wgpu requires Vulkan ICD. Works via `d3d12` paravirtualisation when running with a GUI. Headless WSL2 sessions will fail at window creation.

## Windows Native Build

The recommended way to run therminal on Windows is a native build (not WSLg).

- `scripts/build-windows.sh` — Bash wrapper for WSL. Syncs repo, invokes PowerShell build, copies exe + resources.
- `scripts/build-windows.ps1` — PowerShell script. Auto-finds cargo, bootstraps MSVC, builds, copies to Desktop + `%APPDATA%\therminal\resources`.
- **IPC on Windows**: `socket_path()` returns `\\.\pipe\therminal-<name>` (named pipe) instead of Unix sockets.
- **Known issue**: WDAC/SmartScreen may block build-script executables. Fix: `Add-MpExclusion -Path` on the target dir.

## Code Style

### Module Size
Prefer small, focused modules over large monolithic files. When a file exceeds ~500 lines, consider splitting it into submodules with a clear single responsibility each.

### General
- Keep functions short and focused — if a function needs a section comment, it's probably a candidate for extraction
- No premature abstraction, but do extract when the same pattern appears in 3+ places
- Rust idioms: prefer `?` over `.unwrap()`, use `thiserror` for public error types
- **Config fields must be wired**: if a config struct has a field, code must read it. Don't declare config options that nothing uses. `TherminalConfig` in `therminal-core` is the single source of truth.

## Building & Testing

Run `./scripts/ci.sh` before committing code changes. This runs the same checks as GitHub Actions CI:

```bash
./scripts/ci.sh        # fmt check, clippy, build, test — all workspace
```

Individual steps if needed:
```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo build --workspace
cargo test --workspace
```

## CLI-vs-MCP Usage Policy (for agents and orchestrators)

When driving therminal from an agent (Claude Code, Codex, scripts), pick the
right surface to preserve prompt-cache health:

**Use `therminal` CLI (or the `tn` short alias in `scripts/tn`) for:**
- Polling: `pane list`, `pane peek`, `agents list` — called N times per turn
- Writes: `pane send`, `pane tag/untag`, `session/pane create`
- Shell dashboards: `events --follow | jq`

**Use MCP for:**
- Subscriptions: `terminal://pane/{id}/output`, `therminal://claude/events`
- Blocking waits: `terminal.panes.wait_for_output`
- Conductor tick: `terminal.panes.get_summary` (MCP-only, ~120 B/call)
- Typed structure feeding downstream tool calls: `get_details`, `get_cadence`,
  `find_with_capacity`
- Destructive / Admin operations: `sessions.destroy`, `panes.destroy`

The `tn` alias in `scripts/tn` is a thin wrapper around `therminal`; copy it
to any directory on `$PATH`. Run `tn --help-policy` for a compact reminder.

Full decision table: `docs/cli.md`. Tool-by-tool classification:
`crates/therminal-daemon/src/mcp/tools.rs` module-level doc comment.

**Cache health constraint**: MCP tool calls each create a distinct cache key
segment. A single `terminal.panes.get_content` call on a 80×24 pane returns
~2 KB; five calls = 10 KB/tick. Use `tn pane peek` (~90–500 bytes) for the
common case and reserve `get_content` for when you need the full grid.

## Scope Boundary: Terminal vs Integration

The core architectural decision: **"Does this need bytes-in-flight, or can it work from stored state?"**

If it needs the live PTY stream or GPU surface, it belongs in the terminal. If it works from stored/structured data, it's an external tool that connects via MCP, CLI, or daemon IPC. Cooperative file reads are acceptable when stored state contains signals that stream inference cannot recover (e.g., session titles, subagent lineage).

**Therminal is the platform, not the monolith.** It stays fast and focused on its privileged position — the live PTY stream and GPU surface — while the ecosystem grows around the MCP interface and the three integration surfaces described below.

## Integration Taxonomy

Three surfaces, mutually exclusive. When a planning question arises, apply the tripwire first, then pick the surface.

### 1. Core capabilities

Generic features that apply to any pane regardless of what's running in it. File path / URL / error hotspots, editor fallback chain, `folder_pane_command`, `folder_opener`, keybindings, themes, agent detection, cadence analysis. Lives in `crates/therminal-terminal/` and `crates/therminal-core/`. Users customize via `therminal.toml`, not by writing code. The live PTY stream remains core's exclusive domain.

### 2. Harness crates (`crates/therminal-harness-*/`)

First-class support for specific AI coding harnesses and their private formats. Each harness crate owns its OSC marker grammar (if any), parsers, typed events, and its own `CLAUDE.md`. Core provides a `SequenceInterceptor::register_osc_handler` hook; harness crates claim OSC codes via a central registry in `therminal-terminal` to avoid collisions. A harness crate is dormant when its process is absent from the pane's process map — zero overhead otherwise.

**Target harnesses for first-class support: Claude Code, Codex, Copilot CLI, OpenCode.** Any other harness requires a deliberate decision and a filed issue before a crate is created. The MCP server remains the stable contract for harnesses without a crate — any harness that speaks MCP can drive therminal today without waiting for a dedicated crate. Per-harness setup docs (config snippets, trust tier recommendations, known quirks) live in `docs/integrations/`.

### 3. Pattern packs (`plugins/`)

TOML files of regex patterns + actions (highlight, widget, emit-event) that match against terminal text output. For user-written customizations, uncooperative sources (cargo output, Python tracebacks, logs), and anything not worth a full harness crate. Sharable as copy-paste. AI-authored from the `therminal-plugin` skill with only the skill's docs in context.

### The tripwire

| Question | Surface |
|---|---|
| Does the source cooperate and emit structured data you control? | harness crate |
| Is the source uncooperative and you need to parse its text output? | pattern pack |
| Does the feature apply to any pane regardless of source? | core capability |

### Event bus

All three surfaces publish to `therminal://events` with a common envelope:

```
{ source_class, source_id, kind, pane_id, ts, cursor, body }
```

- `source_class` ∈ `{harness, pattern, core}`
- `source_id`: harness name (`claude`, `codex`, `copilot`, `opencode`), pack name (`cargo-errors`), or core subsystem name
- `kind`: recommended cross-surface vocabulary (`tool_call`, `agent_state`, `progress`, `error`, `waiting`, `done`), plus source-specific kinds

Orchestrators filter by any combination. Adversarial-review use cases subscribe to `source_class=harness` to observe all harness events regardless of origin. The OSC handler registry, pattern engine, and event bus each have their own P1 spec issue (tn-fp42, tn-3vzi, tn-bu1s); this taxonomy is the shared frame those specs build on.

## Task Tracking

Issue tracking via beads (prefix: `tn`).

## Related Projects

- **thermal-desktop** — Linux Hyprland shell, will consume therminal as a dependency
- **TabzChrome** — Browser MCP tools, paired with therminal for complete agent workspace


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd-push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
