# therminal-app

winit window, GPU grid renderer, mouse input, pane management, PTY wiring.

## Module Structure

```
src/
├── main.rs              # Entry point, CLI parsing
├── window/
│   ├── mod.rs           # Event loop, App struct, facade methods
│   ├── mouse.rs         # Mouse input, separator drag, hit-testing
│   ├── keybindings.rs   # Key event dispatch
│   ├── chrome/
│   │   ├── mod.rs           # Chrome trait, re-exports
│   │   ├── status_bar.rs    # Status bar rendering
│   │   ├── pane_header.rs   # Pane header rendering
│   │   ├── tab_bar.rs       # Workspace tab bar rendering
│   │   ├── csd.rs           # Client-side decorations
│   │   ├── colors.rs        # Chrome color helpers
│   │   ├── text_cache.rs    # Chrome text caching (ChromeTextCache type alias)
│   │   ├── render_pass.rs       # Shared `with_chrome_render_pass` helper (tn-ppub)
│   │   ├── delegate_summary.rs  # Delegate result summary in chrome
│   │   └── overlays.rs          # Chrome overlay helpers
│   ├── help_overlay.rs  # Keybinding help overlay
│   ├── settings_overlay/
│   │   ├── mod.rs       # Settings panel overlay entry point
│   │   ├── types.rs     # Settings data types
│   │   ├── nav.rs       # Keyboard navigation logic
│   │   ├── state.rs     # Panel state management
│   │   ├── sections.rs  # Section definitions (Shell, Hotspots, Theme, Accessibility)
│   │   ├── theme.rs     # Theme preset handling
│   │   ├── tests.rs     # Settings overlay tests
│   │   └── renderer/
│   │       ├── mod.rs   # Renderer entry point
│   │       ├── layout.rs # Layout computation
│   │       ├── rects.rs  # Rectangle drawing
│   │       └── text.rs   # Text rendering
│   ├── trust_escalation_overlay.rs # Trust tier escalation dialog
│   ├── toast.rs         # Toast notification overlay
│   ├── event_handler/
│   │   ├── mod.rs       # Top-level event routing: handle_keyboard_input_event, handle_keybinding, mouse/window handlers, rename
│   │   ├── scroll.rs    # scroll_focused_pane, focused_pane_is_scrolled_back (tn-5dpv)
│   │   ├── settings.rs  # apply_settings_command, SettingsCommand dispatch, overlay open/close, trust escalation
│   │   └── pty_input.rs # handle_key_input: winit key → PTY byte encoding
│   ├── init.rs          # Window initialization
│   ├── render_driver.rs # Render orchestration, widget overlay drawing
│   ├── render_tests.rs  # Render driver tests
│   ├── reconcile.rs     # Layout reconciliation
│   ├── git_ref_open.rs  # Git ref hotspot click handling
│   ├── pane_ops/
│   │   ├── mod.rs              # Shared helpers (daemon_rpc, make_pane_callbacks), re-exports
│   │   ├── split_ops/
│   │   │   ├── mod.rs          # split_focused_pane, split_pane_by_id
│   │   │   ├── local.rs        # Local split implementation
│   │   │   ├── remote.rs       # Remote split via daemon IPC
│   │   │   └── remote_helpers.rs # Remote split helper utilities
│   │   ├── close_ops.rs        # close_focused_pane, close_pane_by_id, close_all_panes, kill_pane_remote
│   │   ├── focus_and_nav.rs    # move_focus, swap_focused_pane, zoom_toggle, adjust_ratio, select_pane_remote
│   │   ├── workspace_ops/
│   │   │   ├── mod.rs          # Re-exports, poll_auto_tile, poll_swarm_watcher
│   │   │   ├── restore.rs      # restore_layout
│   │   │   ├── switch.rs       # switch_workspace, send_to_workspace
│   │   │   ├── auto_tile.rs    # Auto-tile event handling
│   │   │   └── swarm.rs        # Swarm watcher polling
│   │   └── editor_clipboard/
│   │       ├── mod.rs          # Re-exports
│   │       ├── clipboard.rs    # copy/paste
│   │       ├── editor.rs       # open_in_editor, plan_open_in_editor
│   │       └── planner.rs      # shell_quote, planning helpers
│   ├── folder_open.rs   # Directory hotspot routing (tn-zqwg)
│   ├── wsl_paths.rs     # WSL2 path translation helpers
│   └── render.rs        # Per-frame rendering, damage tracking
├── pane/
│   ├── mod.rs           # PaneListener, re-exports
│   ├── geometry.rs      # Layout constants, content_area_rect()
│   ├── layout/
│   │   ├── mod.rs       # LayoutNode binary tree, split/merge/focus
│   │   ├── tree.rs      # Tree traversal and manipulation
│   │   └── snapshot.rs  # Layout snapshot serialization
│   ├── workspace.rs     # WorkspaceManager, saved layout snapshots
│   ├── state.rs         # PaneState, PaneStatus, PaneTermSize
│   ├── spawn.rs         # spawn_pane(), PTY reader loop
│   ├── remote_spawn.rs  # Remote pane spawn via daemon IPC
│   ├── swarm_watcher.rs # Agent swarm monitoring
│   ├── backend.rs       # PaneBackend trait, PaneBackendKind (Terminal | WebView)
│   └── auto_tile.rs     # AutoTileDebouncer for agent spawn/exit events
├── widgets/
│   ├── mod.rs              # WidgetId, re-exports
│   ├── gpu.rs              # WidgetRenderer + WidgetManager (freshness cache)
│   ├── rasterizer.rs       # WidgetSpec, WidgetKind, tiny-skia rasterization
│   ├── badge.rs            # AgentBadgeSource (agent status pill, PoC)
│   ├── agent_timeline.rs   # AgentTimelineSource (tool activity bar, tn-x85k)
│   └── pattern_widget.rs   # Pattern-engine widget bridge (tn-068b)
├── cli/
│   ├── mod.rs           # CLI subcommand dispatch
│   ├── pane.rs          # pane subcommands
│   ├── session.rs       # session subcommands
│   ├── workspace.rs     # workspace subcommands
│   ├── agents.rs        # agents subcommands
│   ├── events.rs        # events subcommand
│   ├── semantic.rs       # semantic subcommands
│   ├── layout.rs        # layout subcommands
│   ├── runtime.rs       # runtime helpers
│   └── format.rs        # Output formatting (TSV/JSON)
├── grid_renderer.rs     # wgpu text rendering, glyph cache, rect drawing
├── overlay.rs           # OverlayLayer: two-pass alpha-blended overlay compositor
├── color_mapping.rs     # ANSI color to thermal palette / glyphon RGBA conversion
├── url_detection.rs     # HTTP(S) URL regex detection
├── clipboard.rs         # OSC 52 clipboard integration
├── menu.rs              # Right-click context menu
├── git_state.rs         # Git branch/status detection
├── daemon_spawn.rs      # Daemon auto-spawn logic
├── claude_cwd.rs        # Claude Code cwd resolution
└── mcp_stdio.rs         # MCP stdio bridge to daemon
```

## App-Side Pattern Engine (tn-f9cl)

The app owns a read-only `PatternEngine` sibling to the daemon's engine, stored as `App.pattern_engine: Option<PatternEngine>`. Both instantiate from the same `[patterns]` config. The app runs `process_finalized_line` per visible row during each render frame, converting `Hotspot`-action matches into `TextHotspot`s and `Widget`-action matches into `PatternWidgetMatch`es routed through `GridRenderer.pattern_widget_sink` to `WidgetManager` (tn-068b). On `ConfigChanged` the app re-instantiates its engine.

## Window Controller Facade

`App` in `window/mod.rs` provides facade methods to centralize repeated patterns:

- `get_layout()` / `get_layout_mut()` — active workspace layout access
- `focused_pane()` / `set_focused_pane()` — focus management
- `compute_layout_rect()` — bundles GPU dimensions + config flags
- `relayout_and_redraw()` — atomic layout + resize_all_panes + request_redraw

Note: `get_layout()` borrows all of `self`, so methods needing simultaneous access to `self.grid_renderer` or `self.config` use direct field access instead.

## Two-Pass Render Architecture

Each frame is composited in two GPU passes, both writing to the same swapchain texture with `LoadOp::Load` for the second pass so the grid content is preserved:

1. **Grid pass** — terminal cell content (backgrounds, glyphs, cursor, selection). Driven by `GridRenderer` and `render_panes_recursive` in `window/render.rs`. Each pane is rendered with its own command encoder so per-pane glyphon prepare/render cycles don't clobber the shared atlas before the GPU executes them. Pane headers, separators, and focus borders are drawn here as part of the per-pane sequence because they require glyphon text rendering which has its own prepare/render lifecycle.

2. **Overlay pass** — semi-transparent chrome backgrounds, widget quads, and modal scrims composited via `OverlayLayer` (`overlay.rs`). Quads are collected per-frame, sorted by depth tier, batched into a single vertex buffer, and rendered in one draw call against the existing alpha-blended `rect_pipeline`.

### `OverlayLayer`

`OverlayLayer` is the per-frame collector for the overlay pass. It exposes `push_rect()` and `push_quad()` to add geometry, then `render()` to flush. Quads carry an `OverlayTier`:

- **Chrome (0)** — status bar, tab bar, pane headers, separators, focus borders.
- **Widget (1)** — Phase 6 overlay widgets (context gauges, tool call cards, thinking indicators).
- **Modal (2)** — help overlay, context menus, visual bell, toast notifications.

Tiers are sorted before vertex generation so higher tiers always composite on top of lower tiers. Within a tier, submission order is preserved.

Currently the status bar background and visual bell are routed through `OverlayLayer` as the proof of concept. Foundation helpers `push_focus_border_overlay`, `push_header_bg_overlay`, and `push_separator_overlay` exist in `chrome/overlays.rs` for future migration of pane chrome backgrounds -- they will be wired in once the corresponding text rendering can also be batched. New Phase 6 widgets should push their backgrounds via `OverlayLayer::push_rect` with the `Widget` tier.

## Widget Config Pattern (tn-x85k)

Pre-rasterized overlay widgets use the tn-npd substrate: data source produces a `WidgetSpec` with a `data_hash` and a `WidgetKind`, then `WidgetManager::upsert` rasterizes via tiny-skia only when the hash changes. There is **no `Widget` trait** -- widgets are pure data flows.

**Adding a new widget:**

1. Add a new `WidgetKind` variant in `widgets/rasterizer.rs` with a matching `*Spec` struct.
2. Add a rasterizer function for the new kind (called from `rasterize_to_pixmap`).
3. Create a source module under `widgets/` that owns the data, computes `data_hash()`, and produces `WidgetSpec` instances.
4. Add a `[widgets.<name>]` config subsection in `therminal-core/src/config/mod.rs` under `WidgetsConfig`.
5. Wire into `draw_widget_overlays()` in `render_driver.rs` with position computation and `upsert`.
6. Add a `KeyAction` toggle if the widget should be user-toggleable.
7. Every config field must be read by the rendering code (no dead config).

Shipped widgets: `badge.rs` (agent status pill), `agent_timeline.rs` (tool activity bar), `pattern_widget.rs` (pattern-engine bridge, tn-068b).

## Pattern-Engine Widget Bridge (tn-068b)

Pattern packs can declare `action = "widget"` rules that produce `ResolvedAction::Widget` matches. The app-side bridge in `widgets/pattern_widget.rs` converts these into `WidgetSpec` (Pill rasterization) and routes them through `WidgetManager::upsert` for visible placement.

**Data flow**: `extend_hotspots_from_patterns` in `render.rs` collects Widget-action matches into `GridRenderer.pattern_widget_sink` (a `Vec<PatternWidgetMatch>`) alongside the existing hotspot path. After `render_panes_recursive` completes, `draw_widget_overlays` in `render_driver.rs` drains the sink and calls `WidgetManager::upsert` + `WidgetRenderer::draw` for each match.

**Widget ID allocation**: Pattern-sourced widgets use a deterministic ID derived from `(pane_id, row, start_col)` so the same match at the same screen position reuses the same cache entry. IDs are offset into the `0x5057...` range to avoid collisions with hard-coded widget IDs.

**Lifecycle rules**:
- Widget matches are per-frame: they exist only while the matched text is visible in the pane viewport. Matches that scroll off-screen simply stop being upserted; stale `WidgetManager` entries are retained but not drawn.
- The `pattern_widget_sink` is cleared every frame in `clear_frame_maps()`. Do not assume matches persist across frames.
- v1 maps all pattern `WidgetKind` variants (Badge, Gauge, Sparkline, Card) to `Pill`. Follow-up issues should add native rasterizer variants.
- The daemon's bus publication (`pattern_dispatch.rs`) is unaffected; both the daemon and app process widget matches independently.

## Status Bar

A full-width status bar at the bottom of the window (24px tall) with three sections:

- **Left**: Agent indicator (`[agent: <name>]`) when a process-tree agent is detected.
- **Center**: Current working directory (from OSC 7), with home directory abbreviated to `~`.
- **Right**: Pane dimensions (`cols x rows`) and last command exit code (from OSC 633 D mark), color-coded green (exit 0) or red (non-zero).

**Data flow**: The PTY reader thread in `pane/spawn.rs` drains `InterceptedEvent`s from the `TherminalInterceptor` and `ProcessDetector` results into a shared `Arc<Mutex<PaneStatus>>` on `PaneState`. The render loop reads this to populate `StatusBarInfo` passed to `draw_status_bar()` in `chrome/status_bar.rs`.

**Config**: the status bar is on by default. To hide it along with the rest of the chrome (pane headers, tab bar) press F11 to toggle `KeyAction::FocusMode` (tn-t2yd.2).

## Workspace Tabs

Named workspaces (`WorkspaceManager` in `pane/workspace.rs`) let users group pane layouts under numbered slots. Each workspace independently owns a `LayoutNode` binary tree. Switching workspaces swaps the entire layout; the previous workspace's layout is preserved in memory. Saved layout snapshots (for close-all/restore) also live in `WorkspaceManager`.

**Keybindings**: `Alt+1`..`Alt+9` switch to workspace N; `Alt+Shift+1`..`Alt+Shift+9` send the focused pane to workspace N.

## Pane Operations

`therminal pane create` accepts `--spawn '<cmd>'` (startup command injection) and `--ratio 0.6` (split proportion for the source pane, clamped 0.1..0.9, default 0.5). `therminal pane focus <id>` selects a pane. `therminal pane move <id> --workspace <N>` moves a pane between workspaces. `therminal workspace create --session <S>` creates an empty workspace, and `therminal workspace rename` renames one. Together these provide the minimal Hyprland-style layout scripting surface (tn-ceqw) -- a shell script can create sessions, workspaces, split panes with ratios, focus, and rename without MCP overhead.

- **Pane Swap**: `LayoutNode::swap_pane(a, b)` swaps two leaves in the binary tree. `SwapNext` (`Alt+Shift+Right`) / `SwapPrev` (`Alt+Shift+Left`).
- **Mouse-Drag Separator Resize**: Dragging a pane separator adjusts `split_ratio`. Implemented in `window/mouse.rs`. 6 px hit-test threshold.
- **Window Edge Resize (CSD)**: When `general.use_csd = true` (default on Linux/Windows) the winit window is created with `with_decorations(false)`, so the OS provides no resize grips. `window/mouse.rs` compensates with `ResizeEdge` hit-testing at the four edges + four corners, updates the cursor icon on hover (`N/S/E/W/NE/NW/SE/SW Resize`), and on mouse-down calls `Window::drag_resize_window(direction)` so the compositor handles the interactive resize. Gated on `use_csd` — disabled when native decorations are on. Precedence runs *after* tab bar, status bar, context menu, and pane separator hit-tests, so edge resize is the fallback when no other UI element claims the point.
- **Font Size**: `Ctrl+=` increase, `Ctrl+-` decrease, `Ctrl+0` reset (clamped 8–32 pt).
- **Help Overlay**: `Ctrl+Shift+?` toggles full-window overlay showing all keybindings by category.
- **Context Menu**: Right-click renders a GPU-drawn floating menu with pane actions (split, close, zoom, copy, paste).
- **Directory Hotspots** (tn-zqwg): File-path hotspots whose target stat'd as a directory get a different right-click menu — "Open in new pane", "Open in file manager", "Copy path" — and a different default click action. Detection lives in `therminal_terminal::hotspot_detection::promote_directory_hotspots`, which the renderer calls after `detect_hotspots_from_text_with_wrap` and stat's each `FilePath` hotspot. The is_dir bit is plumbed through `RenderCell.hotspot` and the renderer's `hotspot_map` so the click handler can branch. Click actions live in `window/folder_open.rs`:
  - **Open in new pane**: spawns the configured `hotspots.folder_pane_command` (default `["tfe", "{path}"]`) by splitting the focused pane and writing `cd '/path' && clear && <cmd> '/path'\n` into the new PTY. The command runs as a child of the shell (no `exec`) so the user gets a prompt back when the command exits or is Ctrl+C'd. The `{path}` token is substituted in every argument. If the binary's not on `PATH`, only the `cd` line is sent and a "<cmd> not found — falling back to shell in folder" toast is shown so the user lands in a working shell at the right cwd. Empty `folder_pane_command` skips straight to the file-manager chain.
  - **Open in file manager**: walks `hotspots.folder_opener` (default `[$FILE_MANAGER, xdg-open, nautilus, dolphin, thunar]` on Linux, `[$FILE_MANAGER, open]` on macOS, `[$FILE_MANAGER, explorer]` on Windows). The first entry whose head token resolves on `PATH` wins. Final fallback is `open::that(path)`.
  - **Pure planners**: `plan_folder_pane_open` and `plan_folder_opener` are stubbed-IO pure functions, exhaustively unit-tested for path quoting, single-quote escaping, env-var expansion, missing-binary fallback, and `{path}` substitution.

## PaneBackend Abstraction

`PaneBackend` trait (`pane/backend.rs`) provides a uniform interface over different pane content types. Methods: `write_input()` (deliver keystrokes/paste), `resize()` (update grid dimensions), `get_content()` (extract visible text for MCP/search), and `backend_type()` (identifier string).

`PaneBackendKind` is the concrete enum stored in each `PaneState`:
- **Terminal** — PTY-backed pane using alacritty_terminal `Term`. Holds `Arc<FairMutex<Term>>`, PTY writer, and PTY master.
- **WebView** — stub variant for future wry integration. Stores a URL and a content buffer.

The enum also provides `resize_to_viewport()` which computes grid dimensions from a pixel `Rect` and renderer metrics before delegating to `resize()`.

## Auto-Tiling

`AutoTileDebouncer` (`pane/auto_tile.rs`) subscribes to `AgentRegistry` events via an `mpsc::Receiver<AgentEvent>` and debounces rapid spawn/exit cycles to avoid layout thrashing. On each `poll()` call it drains the event receiver, queues pending actions with timestamps, and yields `AutoTileAction`s once the debounce window expires:

- **Split** — when an agent is registered on a pane, queue a split to create a companion pane (unless one already exists).
- **Reclaim** — when an agent exits, queue removal of the auto-created pane.

If an agent spawns and exits within the debounce window, the two events cancel each other out and no layout change occurs. Debounced actions are forwarded as `UserEvent` variants to the winit event loop so pane operations happen on the main thread.

### Hook-driven subagent auto-tile (tn-s8w3)

`SwarmDebouncer` (`pane/auto_tile.rs`) applies the same debounce pattern to `SwarmWatcherEvent`s. It receives events from two sources:

1. **Hook path** (primary): The per-pane forwarder in `remote_spawn.rs` subscribes to `DaemonEvent::SubagentStarted` / `SubagentStopped`. When the daemon resolves a `subagent_start` hook signal to a pane, the forwarder converts it to `SwarmWatcherEvent::SpawnSubagent` / `ReclaimSubagent` and sends it through `App.swarm_debouncer_tx`. The `swarm_wake` callback sends `UserEvent::SwarmWatcherTick` to poll the debouncer on the main thread.

2. **File scanner** (fallback): The `SwarmWatcher` thread (`pane/swarm_watcher.rs`) polls `~/.claude/projects/*/*/subagents/agent-*.jsonl` every 500ms and detects new/stale subagent files. Events are bridged to the debouncer via the watcher bridge thread.

Dedup is naturally handled: `spawn_subagent_pane()` checks `swarm_panes.contains_key(&agent_id)` before creating a pane, so the faster hook path wins and the file scanner's later discovery is a no-op.
