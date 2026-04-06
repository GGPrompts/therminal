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
│   └── render.rs        # Per-frame rendering, damage tracking
├── pane/
│   ├── mod.rs           # PaneListener, re-exports
│   ├── geometry.rs      # Layout constants, content_area_rect()
│   ├── layout.rs        # LayoutNode binary tree, split/merge/focus
│   ├── workspace.rs     # WorkspaceManager, saved layout snapshots
│   ├── state.rs         # PaneState, PaneStatus, PaneTermSize
│   └── spawn.rs         # spawn_pane(), PTY reader loop
├── grid_renderer.rs     # wgpu text rendering, glyph cache, rect drawing
├── hotspot_detection.rs # File paths, errors, git refs, issue refs
├── url_detection.rs     # HTTP(S) URL regex detection
├── clipboard.rs         # OSC 52 clipboard integration
├── menu.rs              # Right-click context menu
└── mcp_stdio.rs         # MCP stdio bridge to daemon
```

## Window Controller Facade

`App` in `window/mod.rs` provides facade methods to centralize repeated patterns:

- `get_layout()` / `get_layout_mut()` — active workspace layout access
- `focused_pane()` / `set_focused_pane()` — focus management
- `compute_layout_rect()` — bundles GPU dimensions + config flags
- `relayout_and_redraw()` — atomic layout + resize_all_panes + request_redraw

Note: `get_layout()` borrows all of `self`, so methods needing simultaneous access to `self.grid_renderer` or `self.config` use direct field access instead.

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
- **Font Size**: `Ctrl+=` increase, `Ctrl+-` decrease, `Ctrl+0` reset (clamped 8–32 pt).
- **Help Overlay**: `Ctrl+Shift+?` toggles full-window overlay showing all keybindings by category.
- **Context Menu**: Right-click renders a GPU-drawn floating menu with pane actions (split, close, zoom, copy, paste).
