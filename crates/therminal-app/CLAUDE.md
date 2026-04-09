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
│   ├── chrome.rs        # Status bar, tab bar, pane headers, separators
│   ├── help_overlay.rs  # Keybinding help overlay
│   ├── pane_ops.rs      # Split, close, swap, restore operations
│   ├── folder_open.rs   # Directory hotspot routing (tn-zqwg)
│   └── render.rs        # Per-frame rendering, damage tracking
├── pane/
│   ├── mod.rs           # PaneListener, re-exports
│   ├── geometry.rs      # Layout constants, content_area_rect()
│   ├── layout.rs        # LayoutNode binary tree, split/merge/focus
│   ├── workspace.rs     # WorkspaceManager, saved layout snapshots
│   ├── state.rs         # PaneState, PaneStatus, PaneTermSize
│   ├── spawn.rs         # spawn_pane(), PTY reader loop
│   ├── backend.rs       # PaneBackend trait, PaneBackendKind (Terminal | WebView)
│   └── auto_tile.rs     # AutoTileDebouncer for agent spawn/exit events
├── grid_renderer.rs     # wgpu text rendering, glyph cache, rect drawing
├── overlay.rs           # OverlayLayer: two-pass alpha-blended overlay compositor
├── color_mapping.rs     # ANSI color to thermal palette / glyphon RGBA conversion
├── hotspot_detection.rs # File paths, errors, git refs, issue refs
├── url_detection.rs     # HTTP(S) URL regex detection
├── clipboard.rs         # OSC 52 clipboard integration
├── menu.rs              # Right-click context menu
└── mcp_stdio.rs         # MCP stdio bridge to daemon
```

## App-Side Pattern Engine (tn-f9cl)

The app owns a read-only `PatternEngine` sibling to the daemon's engine, stored as `App.pattern_engine: Option<PatternEngine>`. Both instantiate from the same `[patterns]` config. The app runs `process_finalized_line` per visible row during each render frame, converting `Hotspot`-action matches into `TextHotspot`s. Widget-action matches are skipped (deferred follow-up). On `ConfigChanged` the app re-instantiates its engine.

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

Currently the status bar background and visual bell are routed through `OverlayLayer` as the proof of concept. Foundation helpers `push_focus_border_overlay`, `push_header_bg_overlay`, and `push_separator_overlay` exist in `chrome.rs` for future migration of pane chrome backgrounds — they will be wired in once the corresponding text rendering can also be batched. New Phase 6 widgets should push their backgrounds via `OverlayLayer::push_rect` with the `Widget` tier.

## Status Bar

A full-width status bar at the bottom of the window (24px tall) with three sections:

- **Left**: Agent indicator (`[agent: <name>]`) when a process-tree agent is detected and `trust.show_agent_indicator` is enabled.
- **Center**: Current working directory (from OSC 7), with home directory abbreviated to `~`.
- **Right**: Pane dimensions (`cols x rows`) and last command exit code (from OSC 633 D mark), color-coded green (exit 0) or red (non-zero).

**Data flow**: The PTY reader thread in `pane/spawn.rs` drains `InterceptedEvent`s from the `TherminalInterceptor` and `ProcessDetector` results into a shared `Arc<Mutex<PaneStatus>>` on `PaneState`. The render loop reads this to populate `StatusBarInfo` passed to `draw_status_bar()` in `chrome.rs`.

**Config**: `general.show_status_bar` (default `true`) controls visibility. `trust.show_agent_indicator` controls the agent name in the left section.

## Workspace Tabs

Named workspaces (`WorkspaceManager` in `pane/workspace.rs`) let users group pane layouts under numbered slots. Each workspace independently owns a `LayoutNode` binary tree. Switching workspaces swaps the entire layout; the previous workspace's layout is preserved in memory. Saved layout snapshots (for close-all/restore) also live in `WorkspaceManager`.

**Keybindings**: `Alt+1`..`Alt+9` switch to workspace N; `Alt+Shift+1`..`Alt+Shift+9` send the focused pane to workspace N.

## Pane Operations

- **Pane Swap**: `LayoutNode::swap_pane(a, b)` swaps two leaves in the binary tree. `SwapNext` (`Alt+Shift+Right`) / `SwapPrev` (`Alt+Shift+Left`).
- **Mouse-Drag Separator Resize**: Dragging a pane separator adjusts `split_ratio`. Implemented in `window/mouse.rs`. 6 px hit-test threshold.
- **Window Edge Resize (CSD)**: When `general.use_csd = true` (default on Linux/Windows) the winit window is created with `with_decorations(false)`, so the OS provides no resize grips. `window/mouse.rs` compensates with `ResizeEdge` hit-testing at the four edges + four corners, updates the cursor icon on hover (`N/S/E/W/NE/NW/SE/SW Resize`), and on mouse-down calls `Window::drag_resize_window(direction)` so the compositor handles the interactive resize. Gated on `use_csd` — disabled when native decorations are on. Precedence runs *after* tab bar, status bar, context menu, and pane separator hit-tests, so edge resize is the fallback when no other UI element claims the point.
- **Font Size**: `Ctrl+=` increase, `Ctrl+-` decrease, `Ctrl+0` reset (clamped 8–32 pt).
- **Help Overlay**: `Ctrl+Shift+?` toggles full-window overlay showing all keybindings by category.
- **Context Menu**: Right-click renders a GPU-drawn floating menu with pane actions (split, close, zoom, copy, paste).
- **Directory Hotspots** (tn-zqwg): File-path hotspots whose target stat'd as a directory get a different right-click menu — "Open in new pane", "Open in file manager", "Copy path" — and a different default click action. Detection lives in `therminal_terminal::hotspot_detection::promote_directory_hotspots`, which the renderer calls after `detect_hotspots_from_text_with_wrap` and stat's each `FilePath` hotspot. The is_dir bit is plumbed through `RenderCell.hotspot` and the renderer's `hotspot_map` so the click handler can branch. Click actions live in `window/folder_open.rs`:
  - **Open in new pane**: spawns the configured `hotspots.folder_pane_command` (default `["tfe", "{path}"]`) by splitting the focused pane and writing `cd '/path' && clear && exec <cmd> '/path'\n` into the new PTY. The `{path}` token is substituted in every argument. If the binary's not on `PATH`, only the `cd` line is sent and a "<cmd> not found — falling back to shell in folder" toast is shown so the user lands in a working shell at the right cwd. Empty `folder_pane_command` skips straight to the file-manager chain.
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
