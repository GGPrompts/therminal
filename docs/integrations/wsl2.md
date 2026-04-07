# WSL2 → Windows therminal (MCP bridge)

Run therminal as a native Windows GUI while driving it from agents that live
inside WSL2 (Claude Code on Linux, Codex CLI, Aider, etc.). The bridge uses
the existing `therminal mcp` stdio subcommand, which connects to the daemon's
MCP IPC endpoint and forwards JSON-RPC bidirectionally over stdin/stdout.

## What this enables

- A single therminal window on Windows, rendered with native GPU acceleration.
- MCP clients running inside WSL2 drive that window over the 15-tool MCP
  surface (session management, pane ops, resources, subscriptions).
- No X server, no WSLg, no cross-VM socket forwarding. The stdio bridge is
  launched as a WSL child process that crosses into Windows via `binfmt_misc`.

## Prerequisites

1. **Built therminal.exe** — run the native Windows build from WSL:
   ```bash
   ./scripts/build-windows.sh
   ```
   This syncs the repo to `C:\Users\<you>\therminal-build\`, invokes
   `scripts/build-windows.ps1`, and deploys:
   - `therminal.exe` to your Windows Desktop
   - `resources/` to `%APPDATA%\therminal\resources` (shell integration scripts)
   See the [Windows Native Build section in the top-level CLAUDE.md](../../CLAUDE.md).

2. **Running daemon** — launch `therminal.exe` on Windows (double-click the
   Desktop shortcut or `explorer.exe therminal.exe` from WSL). The daemon
   is embedded in the main binary and listens on a Windows named pipe:
   `\\.\pipe\therminal-mcp` by default.

3. **binfmt_misc** — standard on WSL2. Confirm with `file /mnt/c/Windows/System32/notepad.exe`;
   if it's recognised and executable, you're set. This is what lets WSL
   processes `exec()` Windows binaries transparently.

## Find the bridge path

From inside WSL:

```bash
ls /mnt/c/Users/$USER/Desktop/therminal.exe
# or, if you deployed to a custom location:
which therminal.exe 2>/dev/null
```

The canonical path after `build-windows.sh` is
`/mnt/c/Users/<you>/Desktop/therminal.exe`.

## Claude Code MCP config

Add the bridge to `~/.config/claude-code/mcp.json` (or the equivalent
`.mcp.json` in your project root):

```jsonc
{
  "mcpServers": {
    "therminal": {
      "command": "/mnt/c/Users/YOUR_USERNAME/Desktop/therminal.exe",
      "args": ["mcp"]
    }
  }
}
```

Claude Code spawns the command as a subprocess and speaks JSON-RPC over its
stdin/stdout. Everything else — tool dispatch, trust enforcement, session
routing — happens inside the Windows daemon.

## How it works

```
┌─────────────────────┐   stdin/stdout    ┌──────────────────────┐
│ Claude Code (WSL)   │ ───────────────▶  │ therminal.exe mcp    │
│ MCP client          │ ◀─────────────    │ (Windows subprocess) │
└─────────────────────┘                   └──────────┬───────────┘
                                                     │ named pipe
                                                     ▼
                                          ┌──────────────────────┐
                                          │ therminal daemon     │
                                          │ \\.\pipe\therminal-  │
                                          │ mcp                  │
                                          └──────────────────────┘
```

1. Claude Code's MCP client `exec()`s the Windows `.exe` path. WSL's
   `binfmt_misc` handler detects the PE header and routes the exec through
   the Windows NT kernel, so the child is a real Win32 process — not a
   Linux process emulating Windows.
2. `therminal mcp` (see `crates/therminal-app/src/mcp_stdio.rs`) loads
   `TherminalConfig`, calls `McpConfig::resolved_socket_path()`, and on
   Windows opens the named pipe via
   `tokio::net::windows::named_pipe::ClientOptions`.
3. Two `tokio::io::copy` tasks race in a `select!`: one pumps stdin to the
   pipe, one pumps the pipe to stdout. The bridge exits cleanly when either
   side closes.
4. The daemon's MCP server handles the JSON-RPC protocol, tool dispatch,
   and trust-tier enforcement. The bridge is pure plumbing.

## Gotchas

- **Working directory**: Claude Code runs inside WSL with a Linux `$PWD`
  (e.g. `/home/you/projects/foo`). The Windows subprocess inherits that
  as its working directory via `\\wsl.localhost\...` translation. Tools
  that read files from the agent's CWD still see the WSL paths — that's
  usually what you want, but be aware when passing paths in tool arguments.
- **Path format in tool calls**: Prefer `/mnt/c/...` or WSL paths in tool
  arguments if the tool is reading files. Windows-style `C:\Users\...`
  paths only work if the tool is invoking Windows APIs directly.
- **Daemon not running**: Every tool call will fail fast with
  `failed to connect to daemon MCP named pipe`. Launch `therminal.exe`
  first. The daemon is embedded in the main binary, so starting the GUI
  also starts the daemon.
- **Trust tier defaults**: New MCP clients are assigned the default trust
  tier from `[trust]` in `therminal.toml`. Until you bump Claude Code's
  tier, write-level tools (input injection, pane spawn) may be denied.
  The config file lives at `%APPDATA%\therminal\therminal.toml` on Windows.
- **Config lives on the Windows side**: The bridge reads config from the
  *Windows* `%APPDATA%` — not from `~/.config/therminal` in WSL. If you're
  used to editing the Linux config, remember to edit the Windows one for
  this path.

## Troubleshooting

### `failed to connect to daemon MCP named pipe at \\.\pipe\therminal-mcp`

The daemon is not running. Launch `therminal.exe` on Windows. Confirm the
named pipe exists from PowerShell:

```powershell
Get-ChildItem \\.\pipe\ | Where-Object Name -like 'therminal*'
```

### `Is the therminal daemon running?` even though the window is open

Check that the MCP server is enabled in `%APPDATA%\therminal\therminal.toml`:

```toml
[mcp]
enabled = true
socket_path = ""   # empty = default (\\.\pipe\therminal-mcp on Windows)
```

If you've set a custom `socket_path`, make sure the WSL-launched bridge
picks up the same config — it will, because the bridge runs as a Windows
process and reads the Windows `%APPDATA%`.

### Tools return "permission denied" or similar

Trust tier is too low. See the trust configuration docs; bump Claude Code's
assigned tier in `[trust]` and reload.

### The bridge hangs with no output

Run it manually from WSL to see the error:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0.0"}}}' \
  | /mnt/c/Users/$USER/Desktop/therminal.exe mcp
```

You should either see a JSON-RPC `initialize` response, or a connection
error pointing at the named pipe path.

### `binfmt_misc` not handling the `.exe`

Uncommon on modern WSL2, but if `exec()` fails with `ENOEXEC`, check:

```bash
cat /proc/sys/fs/binfmt_misc/WSLInterop
```

If missing, enable WSL interop in `/etc/wsl.conf`:

```ini
[interop]
enabled = true
appendWindowsPath = true
```

Then `wsl.exe --shutdown` and relaunch.

## Smoke test checklist

Manual verification, run from WSL2 with therminal.exe on the Desktop:

1. **Bridge launches**: `/mnt/c/Users/$USER/Desktop/therminal.exe mcp < /dev/null`
   should exit quickly with either a JSON response (if daemon running) or a
   clean "failed to connect" error (if not). A silent hang or `ENOEXEC`
   means binfmt_misc is broken.
2. **Daemon reachable**: With `therminal.exe` running, pipe an `initialize`
   JSON-RPC request into `therminal.exe mcp` and verify you get a response
   frame back.
3. **Claude Code round-trip**: Configure the MCP server entry above,
   restart Claude Code, and run `mcp__therminal__list_sessions` (or the
   equivalent tool). You should see the Windows daemon's session list.
4. **Trust enforcement**: Attempt a write-tier tool from Claude Code
   before bumping trust. You should get a permission error, not a crash.

### Verified during bridge implementation

Running the probe from step 1 against a Desktop-deployed `therminal.exe`
with the daemon stopped produces exactly:

```
Error: failed to connect to daemon MCP named pipe at \\.\pipe\therminal-mcp. Is the therminal daemon running?

Caused by:
    The system cannot find the file specified. (os error 2)
```

This confirms the bridge is reachable from WSL via binfmt_misc, the
Windows `TherminalConfig` loader resolves the default named pipe path, and
the error surface is clean. Steps 2–4 require a running daemon and are
left to the operator for end-to-end validation.

## Related

- `crates/therminal-app/src/mcp_stdio.rs` — bridge implementation.
- `crates/therminal-daemon/CLAUDE.md` — daemon IPC, named pipe layout,
  trust enforcement.
- `scripts/build-windows.ps1` / `scripts/build-windows.sh` — native build.
- Top-level `CLAUDE.md` "Windows Native Build" / "WSL2" sections.
