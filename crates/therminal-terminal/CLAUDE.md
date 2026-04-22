# therminal-terminal

PTY management, OSC parsing, state inference, agent detection, region index.

## Module Structure

```
src/
├── lib.rs
├── pty.rs               # Shell spawn, ShellType detection, SpawnOptions
├── pty_runtime.rs       # PtyPaneCore — shared PTY lifecycle for app + daemon
├── interceptor.rs       # TherminalInterceptor, SequenceInterceptor trait
├── process_detector/
│   ├── mod.rs           # ProcessDetector struct, scan(), public API
│   ├── classifier.rs    # classify_process, classify_wsl_process, AgentType matching
│   └── wsl_probe.rs     # fetch_wsl_ps_stdout, parse_wsl_ps, parse_wsl_ps_tree
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
├── terminal.rs          # TerminalSize shared type, GraphicsEvent variants
├── input.rs             # KeyCode, Modifiers, encode_key() / encode_key_kitty()
├── osc633.rs            # OSC 633 shell integration parser + CommandTracker
├── graphics/
│   ├── mod.rs           # Kitty graphics APC parser, key=value header, response envelope
│   ├── chunk_buffer.rs  # (image_id, placement_id) accumulator with 64 MB hard cap
│   ├── store.rs         # Decoded image cache (tn-0htm)
│   └── placements.rs    # CPU-side placement set with scroll/erase lifecycle (tn-0m3i)
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

## Kitty Graphics (tn-7xme)

`graphics/` is the protocol layer for the Kitty graphics APC format:

```text
ESC _ G <key>=<value>(,<key>=<value>)* ; <base64-payload> ESC \
```

`KittyGraphicsParser` is installed as the APC byte sink on `TherminalInterceptor` — the VTE machine calls `intercept_apc_byte` for each payload byte and `intercept_apc_end` on `ST`/`CAN`/`SUB`. On `apc_end` the parser splits header/payload, resolves the action (`a=t/T/p/d/q/f`), and emits a `crate::terminal::GraphicsEvent`:

- `GraphicsTransmit` — `a=t` / `a=T` / `a=f`, carries the (reassembled) base64 payload plus format/medium/pixel-size metadata. `display=true` when the action was `T` (transmit-and-display).
- `GraphicsDisplay` — `a=p`, with rows/cols/z-index.
- `GraphicsDelete` — `a=d`, with a `DeleteScope::{All, ById}` coarse-grained view (full `d=` subflags stay on the `RawGraphicsCommand` the variant carries).
- `GraphicsQuery` — `a=q`, terminal replies `OK` through the APC response envelope. The canonical feature probe `\x1b_Gi=1,a=q;\x1b\\` gets `\x1b_Gi=1;OK\x1b\\`.

Parsing is **protocol-only** — the parser does **not** base64-decode the payload nor touch pixel bytes. It hands the raw (base64-encoded) bytes plus the `GraphicsFormat` flag to downstream decoders (tn-0htm).

Multi-chunk transmits (`m=1` continuation flag) are accumulated by `chunk_buffer::ChunkBuffer`, keyed on `(image_id, placement_id)`. A **64 MB hard cap** per entry (`CHUNK_BUFFER_HARD_CAP`) guards against a runaway client: on overflow the entry is dropped and the parser emits an `ENOMEM` APC response envelope instead of an event.

Response bytes (`OK` / `EINVAL` / `ENOMEM`) flow out of the interceptor through an optional `mpsc::Sender<Vec<u8>>` set via `TherminalInterceptor::set_graphics_response_sink(...)`. The daemon / app wires this channel to the PTY writer so the producing program sees the reply. The `q=` flag on the incoming command gates emission: `q=0` sends every response, `q=1` sends only errors, `q=2` stays silent.

### Grid-owned image placements (tn-0m3i)

`graphics/placements.rs` adds `PlacementSet`, the CPU-side collection of displayed image instances. The grid (via the downstream `Term` wrapper) owns placements alongside cells and semantic region marks so images:

- Scroll with content (`PlacementSet::scroll_by(delta)` decrements anchor rows; placements whose new row would go negative are dropped).
- Drop off the top of scrollback (`trim_scrollback_below(min_row)`).
- Are cleared when the underlying cells are erased (`clear_rows(start, end)` for CSI J / 2J variants; `clear_cell(row, col)` for single-cell overwrites).
- Stack deterministically — v1 uses a simple **under-text / over-text split**: `z < 0` renders before terminal cells, `z >= 0` after. Within each bucket placements order by `(z_index, created_at)` ascending, so a later `a=p` at the same z paints over an earlier one. Kitty's ultra-low "under-background" tier (`z < -1_073_741_824`) is deliberately **not** implemented in v1.

Pixel-valued `s=` / `v=` / `X=` / `Y=` fields are stored **as-is** on `Placement` (`px_x_offset`, `px_y_offset`). The cell-pixel conversion happens on the render side, which reads `GridRenderer::cell_px()` on `therminal-app`. Keeping conversion out of `therminal-terminal` keeps this crate free of a rendering dependency.

**Delete filter support** (the `d=` refinement on `a=d`):

- `d=a` — delete all placements.
- `d=i` — delete all placements of `image_id`.
- `d=i,p=` — delete a specific `(image_id, placement_id)`.
- `d=C` — delete the newest placement anchored at the cursor cell.
- **TODO** (`tracing::debug` stubs, no mutation): `d=r` (row match), `d=c` (column match), `d=x`/`d=y` (pixel match), `d=z` (z-index match), `d=n` (count-limited delete).

Wiring: `graphics::apply_event_with_placements(store, placements, event, cursor_row, cursor_col)` is the companion to `apply_event()` for callers that own both halves of the pipeline. It routes pixel mutations into `ImageStore` and placement mutations into `PlacementSet` in a single call; `a=T` (transmit-and-display) inserts a placement at the cursor as well as ingesting the pixels.

## Hotspot Detection

`hotspot_detection.rs` scans plain text rows for actionable patterns using compiled regexes. Works on `Vec<String>` rows (no GPU dependency) so both the app renderer and daemon MCP server can use it. Detected hotspot kinds: `FilePath` (with optional `:line:col`), `ErrorLocation` (Rust `-->` and TypeScript `file(line,col)` styles), `GitRef` (7-40 char hex hashes, branch names from `git branch` output), `IssueRef` (`#123`, `PREFIX-456`), and `Url` (HTTP/HTTPS). Higher-priority matches (URLs, error locations) suppress overlapping lower-priority matches (file paths).

`TextHotspot` carries an `is_dir` flag (default `false`). The text-only detection pass never touches the filesystem, but `promote_directory_hotspots(&mut [TextHotspot], stat_fn)` walks the result and sets `is_dir = true` on `FilePath` hotspots whose target stat'd as a directory. The app's renderer calls this with a closure wrapping `std::fs::metadata` so the click handler can route directory hotspots through `folder_pane_command` (default `tfe`) instead of the editor fallback chain (tn-zqwg). Helper `strip_line_col_suffix` strips `:line[:col]` from a path-like string before stat.
