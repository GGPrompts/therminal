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
└── region_index.rs      # Semantic region tagging
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
