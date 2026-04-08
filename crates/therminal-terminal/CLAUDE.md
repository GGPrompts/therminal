# therminal-terminal

PTY management, OSC parsing, state inference, agent detection, region index.

## Module Structure

```
src/
├── lib.rs
├── pty.rs               # Shell spawn, ShellType detection, SpawnOptions
├── pty_runtime.rs       # PtyPaneCore — shared PTY lifecycle for app + daemon
├── interceptor.rs       # TherminalInterceptor, SequenceInterceptor trait
├── process_detector.rs  # Process tree BFS, agent classification
├── state_inference/
│   ├── mod.rs           # AgentStateInference, feed_bytes(), update_command_state()
│   ├── types.rs         # AgentType, InferredStatus, StateChangeNotification
│   ├── patterns.rs      # Compiled regex patterns
│   ├── cadence.rs       # ByteChunkStats, OutputCadence, timing analysis
│   ├── persistence.rs   # State file JSON write/cleanup
│   └── ansi_strip.rs    # Stateful ANSI escape sequence stripper
├── region_index.rs      # Semantic region tagging
├── agent_registry.rs    # AgentRegistry, AgentEntry, AgentEvent, AgentStatus
├── event_log.rs         # Per-session JSONL event logging with truncate rotation
├── terminal.rs          # TerminalSize shared type, DEFAULT_COLS/DEFAULT_ROWS
├── input.rs             # KeyCode, Modifiers, encode_key() / encode_key_kitty()
├── osc633.rs            # OSC 633 shell integration parser + CommandTracker
└── hotspot_detection.rs # File paths, URLs, git refs, issue refs, error locations
```

## Shared PTY Runtime

`pty_runtime.rs` provides `PtyPaneCore<L>` — the shared PTY lifecycle used by both the GUI app and the daemon. It handles:
- `Term::new()` with configurable `EventListener`
- PTY spawn via `spawn_shell_with_options()`
- Reader/writer clone and thread setup
- ANSI processor loop calling a `PtyReaderHandler` trait

The app implements `PtyReaderHandler` with interceptor + process detector + wake callbacks. The daemon implements it with `DaemonEvent::PaneOutput` broadcast.

## Shell Detection

`get_default_shell()` in `pty.rs` probes in order:
- **Linux/macOS**: `$SHELL` env var, fallback `/bin/bash`
- **Windows**: `wsl.exe` > `pwsh.exe` > `powershell.exe` > `ComSpec`/`cmd.exe`

WSL is preferred on Windows because most dev workflows live in WSL. When shell is `wsl.exe`, `--cd ~` is passed so the shell starts in the Linux home directory.

## Agent Registry

`AgentRegistry` (`agent_registry.rs`) is the central registry tracking all detected agents across panes. It combines process detector results with state inference to maintain a real-time map of `PaneId -> AgentEntry`. Each entry records the agent name, `AgentType`, `AgentStatus`, detection timestamp, and optional PID.

Status lifecycle: `Active` (just detected) -> `Idle` | `Processing` | `Streaming` | `Thinking` | `ToolUse` | `AwaitingInput`. Status is updated via `update_status()` which converts from `InferredStatus`.

The registry emits `AgentEvent`s (`Registered`, `Unregistered`, `StatusChanged`) through an `mpsc` channel. Consumers (like the app's `AutoTileDebouncer`) take the receiver via `take_event_rx()` and react to agent lifecycle changes.

## Event Log

`EventLog` (`event_log.rs`) provides per-session structured diagnostics as append-only JSONL files at `$XDG_RUNTIME_DIR/therminal/sessions/<id>.events.jsonl`. Each line is an ISO 8601 timestamped `SessionEvent`:

- `Spawn` (command, cwd), `StatusChange`, `CommandStart`, `CommandFinish` (with exit code + duration), `Resize`, `PtyEof`, `Bell`

Uses a simple truncate-on-overflow rotation: when entry count exceeds `max_entries` (default 5000), the file is truncated and writing restarts. The timestamp implementation is zero-dependency (no chrono).

## Keyboard Input

`input.rs` provides platform-agnostic key types (`KeyCode`, `Modifiers`) and encoding functions that convert key presses into PTY byte sequences:

- `encode_key()` — standard xterm escape sequences for printable text, control characters, cursor/editing keys, and function keys.
- `encode_key_kitty()` — kitty keyboard protocol (progressive enhancement flags 1-2) for unambiguous key encoding with modifier tracking.

## OSC 633 Shell Integration

`osc633.rs` implements a byte-level scanner for the VS Code shell integration protocol. `Osc633Parser` is a stateful parser that finds `ESC ] 633 ; <mark> [; <data>] BEL|ST` sequences in the raw PTY stream without consuming the bytes (the full stream still goes to alacritty_terminal for rendering).

Parsed marks: `A` (PromptStart), `B` (PromptEnd), `C` (PreExec), `D` (CommandFinished with exit code), `E` (CommandLine text).

`CommandTracker` consumes parsed marks and builds a list of `CommandBlock`s representing discrete command executions in the scrollback, each with start/end line, command text, exit code, and lifecycle state.

## Hotspot Detection

`hotspot_detection.rs` scans plain text rows for actionable patterns using compiled regexes. Works on `Vec<String>` rows (no GPU dependency) so both the app renderer and daemon MCP server can use it. Detected hotspot kinds: `FilePath` (with optional `:line:col`), `ErrorLocation` (Rust `-->` and TypeScript `file(line,col)` styles), `GitRef` (7-40 char hex hashes, branch names from `git branch` output), `IssueRef` (`#123`, `PREFIX-456`), and `Url` (HTTP/HTTPS). Higher-priority matches (URLs, error locations) suppress overlapping lower-priority matches (file paths).

`TextHotspot` carries an `is_dir` flag (default `false`). The text-only detection pass never touches the filesystem, but `promote_directory_hotspots(&mut [TextHotspot], stat_fn)` walks the result and sets `is_dir = true` on `FilePath` hotspots whose target stat'd as a directory. The app's renderer calls this with a closure wrapping `std::fs::metadata` so the click handler can route directory hotspots through `folder_pane_command` (default `tfe`) instead of the editor fallback chain (tn-zqwg). Helper `strip_line_col_suffix` strips `:line[:col]` from a path-like string before stat.
