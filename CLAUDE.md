# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

**Phases 0–5 complete; Phase 6 (overlay widgets) in progress. The GUI is a daemon client by default (tn-382v Phase B / tn-beez): split / close / focus route through `IpcRequest::SplitPane` / `KillPane` / `SelectPane`, so MCP `terminal.panes.*` operations act on the visible window and layouts persist across daemon restarts. The GUI auto-spawns `therminal-daemon` on startup if no daemon is running (tn-txs8), with `[daemon] binary_path` as a config override. `mcp.attach_mode = "local"` remains as a one-release escape hatch.** The terminal renders with wgpu, runs a shell, handles keyboard + mouse input, has a SequenceInterceptor for AI-aware OSC parsing, semantic region indexing, shell integration scripts, process tree agent detection with a live agent registry tracking status across panes, output cadence analysis, persistent multiplexed sessions via daemon with socket-as-lock, zero-downtime handoff with SCM_RIGHTS FD passing, debounced session state persistence, tn-zamd structured `PaneStateSnapshot` replay on reattach (mode flags / cursor / visible grid synthesized back into the GUI's local `Term` so TUI mouse capture + cursor visibility survive GUI restarts), IPC protocol (MessagePack framing), split panes with mouse-drag separator resize, auto-tiling for agent swarms (hook-driven via `SubagentStarted`/`SubagentStopped` daemon events from Claude Code hooks with JSONL file scanning as fallback, tn-s8w3), PaneBackend abstraction (Terminal | WebView | JsonlTail | RemotePty) with live WebView pane embedding via wry (tn-s5vj, sidecar mode — loads URLs in platform-native webviews: WebKitGTK on Linux, WebView2 on Windows, WKWebView on macOS), pinned/sticky panes that persist across workspace switches (tn-n5jk, `Ctrl+Alt+P` toggle, `terminal.panes.pin`/`unpin` MCP tools, first-class bool + tag persistence hybrid), workspace tabs with inline rename, pane swap, font size keybindings, keybinding help overlay (auto-sizing with two-column fallback), shell profile launcher overlay with tile grid and Nerd Font icons (tn-5c9u, `Ctrl+Shift+L`, profiles from `[profiles.*]` config with `icon`/`color` fields, daemon `SplitPane.profile` parameter wired through MCP + CLI), right-click context menus (including Copy pane ID), click-to-open hotspots (file paths, URLs, errors) with wrapped-line joining, pointer cursor affordance, configurable editor fallback chain and toast feedback on failures, directory hotspots routed through `folder_pane_command` (default `tfe`) for in-pane spawn and `folder_opener` chain for "reveal in file manager" (tn-zqwg), F11 focus mode toggle (hides all chrome), cwd inheritance on pane split, TOML config with hot-reload, JSONL event logging, control mode, MCP Resources with subscription-based pane content streaming, live Claude Code session observability via `/tmp/claude-code-state/` watcher + `~/.claude/projects/` JSONL tailers (parent + subagents, `TaggedAgentEvent` streamed over `therminal://claude/events`, plus a `claude-events` CLI viewer dev tool), agent lifecycle events streamed over `therminal://agents/events`, OSC 633 command transcript exposed via `terminal.semantic.query_commands`, window edge resize + cursor affordances under CSD mode, and opaque per-pane key/value tag bag (tn-bbvf) for binding panes to external concepts (issue ids, branches, worker ids) — set via `terminal.panes.tag` / `terminal.panes.untag`, surfaced in `terminal.panes.list`, and persisted across daemon restarts, an MCP server exposing 34 tools across 7 domains with trust tier enforcement, a shipped semantic pattern matching engine (tn-yrjd) wired into PTY line finalization and OSC 133 C (tn-86us) with live `emit_event` dispatch and authoring via TOML pattern packs (see `docs/pattern-matching-spec.md` and the bundled `therminal-plugin` Claude Code skill under `resources/skills/`), harness-resolved Claude Code tool-call hotspots that resolve relative paths against the agent's cwd across worktree hops (tn-gidy), delegate profile schema under `[delegate.profiles.*]` for planned sibling-Claude spawning (tn-ztv3.1, schema only), delegate result capture via `terminal.panes.capture_result` using transcript-first with grid fallback (tn-ztv3.5), and a cache-friendly `therminal pane|session|workspace|agents|events|semantic` CLI surface (tn-k13n) that wraps the daemon-client for MCP consumers (Claude Code, Codex, scripts) to drive the same daemon without paying MCP framing costs — terse TSV by default (`pane list` for 5 panes is ≤ 150 bytes), `--json` for structured callers, see `docs/cli.md`.

## Architecture

Cargo workspace with ten crates. Each crate has its own `CLAUDE.md` with deep architecture docs.

```
crates/
├── therminal-protocol/           # Wire types, MCP schema, semantic events
├── therminal-terminal/           # PTY, OSC parsing, state inference, agent detection, region index
├── therminal-core/               # Color palette, wgpu context, text renderer, TOML config, hot-reload
├── therminal-runtime/            # Cross-platform paths, runtime dir management
├── therminal-daemon/             # Session manager, event bus, multiplexer, MCP server, trust enforcement
├── therminal-daemon-client/      # IPC client library (framing, socket transport)
├── therminal-harness-claude/     # First-class Claude Code integration (JSONL tailer + state watcher + event stream)
├── therminal-tui/                # Ratatui TUI dashboard binary (sessions, panes, agents pages)
├── therminal-integration-tests/  # End-to-end integration test scenarios
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
- **WebView**: wry (platform-native: WebKitGTK / WebView2 / WKWebView)
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

- **WSL cwd tracking (tn-kkr8)**: Shell integration scripts emit OSC 9;9 (`\e]9;9;<windows-path>\e\\`) alongside OSC 7 when `WSL_DISTRO_NAME` is set. The payload is produced by `wslpath -w "$PWD"` inside WSL, giving the daemon a Windows-native path (UNC `\\wsl.localhost\...` or drive `C:\...`) without needing `linux_to_unc()` translation. OSC 7 + `linux_to_unc()` remains as fallback for shells without the updated integration scripts. The `InterceptedEvent::WslCwd` variant is wired through all the same cwd update paths as `CurrentDirectory`.
- **Windows home abbreviation**: `abbreviate_path()` in `chrome.rs` detects WSL2 via `WSL_DISTRO_NAME` and abbreviates `/mnt/c/Users/<user>` to `~win` using `USERPROFILE` env var.
- **Process detector false positives**: `is_wsl2_interop_process()` in `process_detector/classifier.rs` filters out Windows `.exe` files launched via binfmt_misc interop before agent classification.
- **GPU**: wgpu requires Vulkan ICD. Works via `d3d12` paravirtualisation when running with a GUI. Headless WSL2 sessions will fail at window creation.

## Windows Native Build

The recommended way to run therminal on Windows is a native build (not WSLg).

**Primary dev setup**: therminal runs as a native Windows binary (`therminal.exe` + `therminal-daemon.exe`) with WSL2 (Ubuntu-24.04) as the default shell. Code is edited in WSL2 (`~/projects/therminal`), built via cross-compilation to Windows, and the binaries are copied to the Windows Desktop. This means debugging often involves both sides: Windows-native process behavior (named pipes, `%APPDATA%`, taskkill) and WSL2-side state (PTY, shell integration, `/tmp/` state files, `~/.claude/`).

### Build & Dev Scripts

- `scripts/build-windows.sh` — Bash wrapper for WSL. Syncs repo, invokes PowerShell build, copies exe + resources.
- `scripts/build-windows.ps1` — PowerShell script. Auto-finds cargo, bootstraps MSVC, builds, copies to Desktop + `%APPDATA%\therminal\resources`.

### Windows Desktop Shortcuts

All live at `C:\Users\marci\Desktop\` (accessible from WSL at `/mnt/c/Users/marci/Desktop/`):

| Script | What it does |
|---|---|
| `build-therminal-windows.cmd` | Invokes `build-windows.ps1` via the WSL UNC path |
| `kill-therminal.cmd` | `taskkill /F` both `therminal.exe` and `therminal-daemon.exe`, verifies no processes remain |
| `wipe-therminal-sessions.cmd` | Deletes `%APPDATA%\therminal\sessions.json` (must kill daemon first or it re-saves) |
| `reset-and-trace-therminal.cmd` | All-in-one: kill → wipe sessions → start daemon in new window → launch GUI with `RUST_LOG` trace to `%TEMP%\therminal-wlu6.log` |
| `run-therminal-trace.cmd` | Launch GUI with trace logging (daemon must already be running), logs via PowerShell `Tee-Object` |
| `therminal.lnk` | Normal launch shortcut |

**Typical dev cycle**: edit in WSL2 → double-click `build-therminal-windows.cmd` → `kill-therminal.cmd` → double-click `therminal.lnk` (or `reset-and-trace-therminal.cmd` for debugging).

### Platform Notes

- **IPC on Windows**: `socket_path()` returns `\\.\pipe\therminal-<name>` (named pipe) instead of Unix sockets.
- **Known issue**: WDAC/SmartScreen may block build-script executables. Fix: `Add-MpExclusion -Path` on the target dir.

### WSL pane observability (tn-966s)

When the daemon runs as a Windows native process **and** the pane's shell is `wsl.exe`, the Claude harness and the daemon-side process detector resolve their inputs through the WSL virtual filesystem so the agent observability pipeline (chrome badges, status bar, auto-tile, `therminal://claude/events`, `terminal.agents.list`) keeps working out of the box. The behavior is implemented in three layers:

1. **OSC 1341 markers (primary)** — Claude Code hook scripts emit OSC 1341 markers (`session_id`, `state`, `cwd`, `context_percent`, `model`, `tool`, `environment`, `subagent_start`/`subagent_stop`) through the PTY stream, which crosses the WSL→Windows boundary transparently. Markers arrive at the exact pane they belong to — no PID matching, no UNC paths. The `environment` field (tn-ncmj) carries cooperative self-identification (`wsl:<distro>`, `docker:<hostname>`, `ssh:<hostname>`, or `local`) so the daemon knows the signal origin without crossing process boundaries. The daemon's marker→capacity bridge updates `PaneCapacityCache` directly. When marker data is fresh (< 30s), file-polled updates are suppressed.
2. **State files (fallback)** — `crates/therminal-harness-claude/src/state.rs` uses `therminal_runtime::wsl::detect_default_distro()` (cached `wsl.exe -l -q`) to rewrite `/tmp/claude-code-state` etc. to UNC paths. UNC watching is best-effort: falls back from `recommended_watcher` to `PollWatcher` (5s interval) and finally skips directories that are unreachable (tn-ag7d). `SESSION_MAX_AGE` is shortened to 5 minutes on Windows (vs 2 hours on Unix) and `pid_is_alive` uses `OpenProcess` for Win32 PIDs (tn-62lq).
3. **JSONL transcripts (fallback)** — `crates/therminal-harness-claude/src/jsonl_tailer.rs::home_dir` consults `therminal_runtime::wsl::detect_wsl_home()` to resolve `~/.claude/projects/{hash}/...` against the WSL user's home. Directory walk failures are logged at WARN and return empty results rather than propagating errors.
4. **Process detection** — `crates/therminal-terminal/src/process_detector/mod.rs::ProcessDetector::with_wsl_distro` flips the scanner into WSL probe mode, where `scan()` shells out to `wsl.exe -d <distro> -e ps -eo pid=,ppid=,comm=,args=` and parses the result through the same classifier as the sysinfo path. The daemon's `process_detector_task` activates this mode automatically when `pane_detector_specs` reports `shell_command == "wsl.exe"` (basename match, case-insensitive).

All paths fall back gracefully on Linux/macOS daemons, on WSL-hosted daemons, and on pure-Windows panes (cmd, pwsh, powershell). WSL helpers (`detect_default_distro`, `detect_wsl_home`, `linux_to_unc`, `is_safe_distro_name`) live in `therminal-runtime::wsl` with a single `OnceLock` per process (tn-9ixz). State→pane resolution uses a three-tier cascade: PID match → `session_id` lookup in capacity cache → cwd fallback with trailing-slash normalization (tn-y2d8, tn-019y).

## Code Style

### Module Size
Prefer small, focused modules over large monolithic files. When a file exceeds ~500 lines, consider splitting it into submodules with a clear single responsibility each.

### General
- Keep functions short and focused — if a function needs a section comment, it's probably a candidate for extraction
- No premature abstraction, but do extract when the same pattern appears in 3+ places
- Rust idioms: prefer `?` over `.unwrap()`, use `thiserror` for public error types
- **Config fields must be wired**: if a config struct has a field, code must read it. Don't declare config options that nothing uses. `TherminalConfig` in `therminal-core` is the single source of truth.

## Theming

Therminal renders both terminal cells **and** chrome (pane headers, separators, focus border, status bar, tab bar, CSD buttons, hotspot underlines, selection, cursor) through a single runtime palette. Hot-reloading `[colors]` re-skins everything in one shot.

### Architecture (tn-g7oo)

`therminal_core::palette::ChromePalette` is the runtime, theme-aware substitute for the compile-time `Color::*` constants chrome modules used to read. Each field is an `[f32; 4]` RGBA value with the alpha already baked in. The struct is owned by `GridRenderer.chrome_palette` and rebuilt on every `apply_color_overrides` call (which is wired into the existing config hot-reload pipeline).

Chrome modules read directly from `&renderer.chrome_palette.<role>`. There is no extra accessor layer — fields are public and addressing is by name.

### Roles

| Field | Default derives from | Used by |
|---|---|---|
| `focus_border` | `Color::FOCUS` @ α=0.92 | 3 px outline around the focused pane, separator-focus, tab-active underline |
| `separator` | `Color::LINE` | Split-pane separator line (unfocused) |
| `separator_focus` | `Color::FOCUS` @ α=0.82 | Separator adjacent to the focused pane |
| `header_bg` | `Color::VOID_2` | Pane header — focused (also tab-active background) |
| `header_bg_dim` | `Color::VOID_0` | Pane header — unfocused |
| `exit_ok` / `exit_error` | `STATUS_OK` / `STATUS_ERROR` @ α=0.90 | Left-edge exit-code stripe in the pane header |
| `status_bar_bg` | `Color::VOID_0` | Bottom status bar fill (also tab-bar fill by default) |
| `tab_bar_bg` | `status_bar_bg` | Workspace tab bar fill — overrides `status_bar_bg` for the tab bar only |
| `tab_active_bg` | `header_bg` | Active workspace tab background |
| `tab_active_underline` | `focus_border` | 2 px underline beneath the active workspace tab |
| `csd_close` | `[0.85, 0.25, 0.25, 1.0]` (red) | CSD close-button hover tint |
| `csd_button_hover` | `[1.0, 1.0, 1.0, 0.1]` (white α=0.1) | CSD min/max/settings hover tint |
| `menu_hover_bg` | `Color::FOCUS` @ α=0.35 | Hovered/selected row tint in the right-click context menu (read by `menu.rs`); dimmed accent so the highlighted row stays distinct from menu text |
| `chrome_fg` | `Color::INK` | Primary chrome text — pane header process, status bar center, tab labels (active), CSD icons |
| `chrome_fg_muted` | `Color::INK_MUTED` | Pane indices, button labels, status bar muted text, inactive tab labels |
| `chrome_fg_focus` | `Color::FOCUS` | Workspace number, agent indicator, claude badge, topic-branch text |
| `chrome_fg_warn` | `Color::WARN` | Git detached HEAD label |
| `chrome_fg_alert` | `Color::ALERT` | Pane header close-button glyph |
| `selection` | `Color::ACCENT_COOL` @ α=0.45 | Terminal text selection rect |
| `cursor` | `Color::WHITE_HOT` @ α=0.85 | Cursor block / underline |
| `hyperlink` | `Color::ACCENT_COOL` | OSC 8 + regex URL underlines (and the linked-cell glyph color); override independently via `chrome_hyperlink` |
| `hotspot_filepath` | `Color::ACCENT_NEUTRAL` | Dotted underline for `FilePath` hotspots |
| `hotspot_url` | `Color::ACCENT_COOL` | Dotted underline for `Url` hotspots (also drives `hyperlink` unless `chrome_hyperlink` is set) |
| `hotspot_error` | `Color::STATUS_ERROR` | Dotted underline for `ErrorLocation` hotspots |
| `hotspot_gitref` | `Color::HOT` | Dotted underline for `GitRef` hotspots |
| `hotspot_issueref` | `[0.706, 0.557, 1.0, 1.0]` (purple) | Dotted underline for `IssueRef` hotspots |

### Authoring a theme

Themes live in `[colors]` in `therminal.toml`. Every field is optional — set only the roles you want to recolor; the rest derive from the bundled Codex 2031 palette.

```toml
[colors]
# Terminal cells (existing fields, unchanged)
background = "#fbf6ec"
foreground = "#2c2823"
selection  = "#d6c8a6"

# Chrome roles (tn-g7oo)
chrome_focus_border  = "#2563eb"   # focus outline + active tab underline
chrome_separator     = "#cbb88a"   # split-pane separator line
chrome_header_bg     = "#f3ead6"   # focused pane header
chrome_header_bg_dim = "#ece2c4"   # unfocused pane header
chrome_status_bar_bg = "#ece2c4"   # bottom status bar (also tab bar)
chrome_tab_active_bg = "#f3ead6"   # active workspace tab
chrome_csd_close     = "#cc3333"   # close-button hover tint
chrome_menu_hover_bg = "#2563eb"   # context-menu hover row (alpha kept @ 0.35)

# Text colors that ride the new background — set these whenever you re-skin
# the chrome backgrounds, or pane labels become unreadable.
chrome_fg            = "#2c2823"   # primary chrome text
chrome_fg_muted      = "#6b5f53"   # secondary text / button labels
chrome_fg_focus      = "#2563eb"   # workspace number / agent indicator
chrome_fg_warn       = "#b8860b"   # git detached HEAD
chrome_fg_alert      = "#cc3333"   # close button glyph

# Hotspot underline colors
hotspot_filepath = "#0d9488"
hotspot_url      = "#2563eb"
hotspot_error    = "#dc2626"
hotspot_gitref   = "#b8860b"
hotspot_issueref = "#7c3aed"
```

A few derivation rules to know:

- **`chrome_focus_border` propagates** to `separator_focus` and `tab_active_underline` automatically.
- **`chrome_header_bg` propagates** to `tab_active_bg`.
- **`chrome_status_bar_bg` propagates** to `tab_bar_bg`. To re-skin only the tab bar, set `chrome_tab_bar_bg` (which does not propagate).
- **`hotspot_url` propagates** to the OSC 8 `hyperlink` underline so hand-rolled and regex-detected URLs match. Set `chrome_hyperlink` to override only the OSC 8 underline without affecting the click-to-open URL hotspot color.
- **`cursor`, `selection`, and `menu_hover_bg` overrides** keep their default alpha (0.85 / 0.45 / 0.35) — only the RGB channels are replaced. This guarantees a recolor of the context-menu hover row stays a tint over the menu backdrop instead of an opaque bar that fights the foreground glyphs.

Invalid hex strings fall back to the default for that single role; the rest of the palette is unaffected.

### Implementation pointers

- `crates/therminal-core/src/palette.rs` — `ChromePalette` struct + `Default` impl + tests.
- `crates/therminal-core/src/config/mod.rs` — `ColorsConfig` chrome/hotspot fields + `chrome_palette()` resolver + tests.
- `crates/therminal-app/src/grid_renderer.rs` — `GridRenderer.chrome_palette` field, rebuilt on every `apply_color_overrides` call.
- `crates/therminal-app/src/window/chrome/{colors,csd,pane_header,status_bar,tab_bar,overlays}.rs` — chrome modules read `renderer.chrome_palette.*` directly.
- `crates/therminal-app/src/menu.rs` — right-click context menu reads `chrome_palette.menu_hover_bg` for the hovered/selected row tint.
- `crates/therminal-app/src/color_mapping.rs` — `hotspot_kind_color(kind, &chrome_palette)` resolves kind-specific underline colors.

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
- Layout: `pane focus/move/swap/resize`, `workspace create/rename/switch`
- Destroy: `pane destroy`, `session destroy` (no trust-tier gate)
- Hook push: `agent-event push` (from Claude Code hook scripts)
- Batch layout: `layout batch` (atomic multi-op from stdin)
- Shell dashboards: `events --follow | jq`

**Use MCP for:**
- Subscriptions: `terminal://pane/{id}/output`, `therminal://claude/events`
- Blocking waits: `terminal.panes.wait_for_output`
- Conductor tick: `terminal.panes.get_summary` (MCP-only, ~120 B/call)
- Typed structure feeding downstream tool calls: `get_details`, `get_cadence`,
  `find_with_capacity`
- Trust-gated destructive ops: `sessions.destroy`, `panes.destroy` (MCP
  enforces trust-tier policy; CLI equivalents skip the gate)

The `tn` alias ships as `scripts/tn` (bash), `scripts/tn.cmd` (CMD), and
`scripts/tn.ps1` (PowerShell). Copy the appropriate wrapper to a directory
on `$PATH`. Run `tn --help-policy` for a compact reminder.

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
