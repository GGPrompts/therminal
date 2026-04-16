# therminal-tui

Ratatui TUI dashboard for the therminal daemon. Standalone binary that connects via IPC.

## Architecture

```
src/
├── main.rs          # CLI entry point (clap + tracing)
├── app.rs           # App state, event loop, tab bar (ported from thermal-desktop)
├── backend.rs       # Sync facade over async DaemonClient (background tokio thread)
├── palette.rs       # Color constants for the TUI (dark theme)
└── pages/
    ├── mod.rs       # TuiPage trait
    ├── sessions.rs  # Sessions page — side-panel layout (list + detail)
    ├── panes.rs     # Panes page — flat pane list with capture preview
    └── agents.rs    # Agents page — live agent status table
```

## Key Design Decisions

- **Synchronous TUI, async IPC**: The crossterm event loop is sync. A background thread runs a tokio runtime that owns the `DaemonClient` connection. Requests and responses flow through `mpsc` channels.
- **Side-panel layout**: The Sessions page uses a left list / right detail split (40/60), matching the thermal-desktop pattern the user values.
- **Standalone binary**: No special wiring into the daemon — connects via the same IPC socket as the GUI and CLI.
- **Graceful degradation**: Works in any terminal emulator. When run inside a therminal pane, file paths in output get hotspots for free.

## Dependencies

- `ratatui` + `crossterm` for TUI rendering
- `therminal-daemon-client` for IPC
- `therminal-protocol` for wire types
- `therminal-runtime` for socket path resolution
- `tokio` for the async IPC worker thread
- `clap` for CLI parsing

## Running

```bash
# Default — auto-detects daemon socket
cargo run -p therminal-tui

# Custom socket path
cargo run -p therminal-tui -- --socket /path/to/daemon.sock
```

## Render loop (tn-dyo1)

The run loop in `app.rs::run` is **dirty-flag gated** — it does NOT
repaint on every iteration. The original implementation called
`terminal.draw()` once per loop and `event::poll(250ms)` immediately
after, which meant:

- Every key event triggered a redraw (fine).
- Every mouse event — including `MouseEventKind::Moved` and `Drag`
  bursts during a click-drag — triggered a redraw, producing visible
  flicker on input bursts.
- The hardware cursor was never hidden, so ratatui's per-frame
  `set_cursor_position` made the cursor visibly jump on every paint
  even when no cells changed.
- `tick()` returned no signal about whether it actually fetched new
  data, so the loop couldn't tell a meaningful refresh apart from a
  throttled no-op.

The fix:

1. **`terminal.hide_cursor()` on entry** — the `ScreenGuard::Drop`
   already runs `Show` to restore it. Without this the cursor flashes
   between every paint.
2. **`TuiPage::tick` returns `bool`** — `true` only when the page
   actually fetched data. The page-level 2 s throttle is unchanged;
   the bool just lifts the signal up to the loop.
3. **Dirty flag** — set on key, actionable mouse (`Down`/`Scroll*`,
   never `Moved`/`Drag`/`Up`), `Resize`, and any `tick()` that
   returned `true`. Cleared after each `terminal.draw()`.
4. **Cadence constants** — `POLL_INTERVAL = 100ms`,
   `MIN_REDRAW_INTERVAL = 16ms`, `IDLE_REDRAW_INTERVAL = 500ms`.
   Idle paints at 2 Hz; bursts coalesce to ≤ 60 fps; input wakeups
   never force a frame on their own. Guarded by
   `redraw_cadence_sane()` in `app.rs::tests`.

If you add a new `TuiPage`, return `true` from `tick()` only when
state changed. An always-true return defeats the gate and the
flicker comes back.

Layout constraints in every page (`sessions.rs`, `panes.rs`,
`agents.rs`) were audited and found to be content-independent —
all `Constraint::Length(n)` values are literals, all
`Constraint::Min(n)` are floors with `Percentage(..)` siblings or
single growable columns. If you add a constraint derived from IPC
data (session count, capture width, etc.), clamp + quantize it
(e.g. `.max(MIN).min(MAX)` and round up to the nearest multiple of
4) so jitter within a step doesn't cause reflow.
