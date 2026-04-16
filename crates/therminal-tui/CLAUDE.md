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
