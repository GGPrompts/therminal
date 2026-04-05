# Therminal App/Core Audit

Date: 2026-04-05
Commit reviewed: `7661c93`
Scope: `crates/therminal-app`, `crates/therminal-core`

## Top Findings

### 1. Shared renderer caches are reused across all panes, which can leak stale rows and cursor state between split panes

Files:

- `crates/therminal-app/src/window.rs:1121`
- `crates/therminal-app/src/grid_renderer.rs:579`
- `crates/therminal-app/src/grid_renderer.rs:681`
- `crates/therminal-app/src/grid_renderer.rs:826`
- `crates/therminal-app/src/grid_renderer.rs:943`

`render_single_pane` renders every pane through one `GridRenderer` instance and only changes its padding before each pass, but `GridRenderer` keeps pane-local render state in shared fields like `row_cache`, `cell_buffers`, `last_cursor_pos`, and `hyperlink_map`. That makes the renderer correct for a single terminal surface but unsafe for sequential multi-pane rendering: after one pane resets damage, partial updates in the next pane can reuse cached rows or cursor metadata from the previous pane. The likely regression is stale text, selections, or cursor artifacts appearing in the wrong split. From a maintainability perspective, this is also the main structural risk in the current pane renderer because it couples cache lifetime to the window instead of the pane.

### 2. Mouse wheel, drag, motion, and reporting are still resolved against the focused pane instead of the pane under the pointer

Files:

- `crates/therminal-app/src/window.rs:761`
- `crates/therminal-app/src/window.rs:782`
- `crates/therminal-app/src/window.rs:845`
- `crates/therminal-app/src/window.rs:857`
- `crates/therminal-app/src/window.rs:889`
- `crates/therminal-app/src/window.rs:894`

Pointer-to-grid conversion goes through `pixel_to_grid`, which resolves coordinates against `focused_pane`, and the code also reads mouse mode flags from the focused pane. In a split layout that means scroll and pointer events can be delivered to the wrong PTY until focus changes, and wheel handling can scroll the focused pane even while the pointer is over a different pane. Left-click focus switching masks part of this path, but wheel, drag, and motion still follow the wrong target. This is a concrete behavioral bug in multi-pane use and will get harder to fix cleanly if more pane-local interaction features are added on top of the current routing.

### 3. The config model advertises settings that the app/core path does not actually apply

Files:

- `crates/therminal-core/src/config.rs:115`
- `crates/therminal-app/src/window.rs:910`
- `crates/therminal-app/src/window.rs:1124`

`TherminalConfig` exposes runtime settings for padding, colors, keybindings, shell/env behavior, and scrollback, but the app/core hot-reload path only applies window title and font changes. Rendering and hit-testing still hardcode `4.0` padding, and there is no corresponding app/core path for color overrides or configurable keybindings. Even where behavior exists elsewhere, the app/core layer presents a broader supported surface than it actually honors. That creates silent misconfiguration rather than explicit failure, which is a maintainability risk because the config schema, docs, and UI behavior can drift apart without tests catching it.

## Verification

Executed:

```bash
cargo test --workspace
```

Result: passed.
