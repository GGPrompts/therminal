# Control Mode Protocol

Control mode provides a text-based, machine-readable protocol for driving
Therminal programmatically, similar to tmux's `-CC` mode. External tools
(claude-squad, agent-deck, etc.) connect via the daemon's IPC socket and
exchange text commands.

## Connecting

Start control mode via the CLI:

```bash
therminal-daemon --control-mode
```

Or connect directly to the Unix domain socket and send the handshake line:

```bash
socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/therminal/daemon.sock" <<< 'mode: control'
```

## Handshake

On connect, the client sends a single line:

```
mode: control\n
```

The daemon responds with a greeting wrapped in a `%begin`/`%end` block at
request ID 0:

```
%begin 0
{"mode":"control","version":"0.1.0","build_hash":"abc1234-1700000000"}
%end 0
```

This confirms the connection is in control mode and provides the daemon's
version and build hash for compatibility checking.

## Response Format

Every command receives a response wrapped in `%begin`/`%end` with an
incrementing request ID (starting from 1):

### Success

```
%begin <request_id>
<JSON or plain text body>
%end <request_id>
```

### Error

```
%begin <request_id>
%error <message>
%end <request_id>
```

Request IDs increment by 1 for each command sent on a connection. The
client can use them to correlate responses with commands.

## Commands

### `new-session [-n NAME]`

Create a new session with an optional name.

```
new-session -n mywork
```

Response:

```json
{"session_id": 42}
```

### `list-sessions`

List all active session IDs.

```
list-sessions
```

Response:

```json
[42, 43]
```

### `send-keys PANE_ID KEYS...`

Send input text to a pane. Everything after the pane ID is treated as
the keys to send (spaces preserved).

```
send-keys 1 ls -la
```

Response:

```json
{"pane_id": 1, "sent": true}
```

### `split-pane [-h|-v] PANE_ID`

Split a pane. Default is vertical (`-v`). Use `-h` for horizontal.

```
split-pane -h 1
```

Response:

```json
{"new_pane_id": 2}
```

### `select-pane PANE_ID`

Focus a pane.

```
select-pane 2
```

Response:

```json
{"pane_id": 2, "selected": true}
```

### `capture-pane PANE_ID [-p]`

Capture the content of a pane. Without `-p`, returns JSON with grid data,
cursor position, and dimensions. With `-p`, returns plain text (one line
per row).

```
capture-pane 1
```

JSON response:

```json
{
  "pane_id": 1,
  "cols": 80,
  "rows": 24,
  "cursor_col": 0,
  "cursor_line": 5,
  "lines": ["$ ls", "file.txt", ""]
}
```

Plain text response (`capture-pane 1 -p`):

```
$ ls
file.txt
```

### `kill-pane PANE_ID`

Close a pane and its PTY.

```
kill-pane 1
```

Response:

```json
{"pane_id": 1, "killed": true}
```

### `list-panes SESSION_ID`

List all panes in a session with their dimensions.

```
list-panes 42
```

Response:

```json
[{"pane_id": 1, "cols": 80, "rows": 24}, {"pane_id": 2, "cols": 80, "rows": 24}]
```

### `ping`

Health check. Returns daemon status.

```
ping
```

Response:

```json
{"status": "ok", "version": "0.1.0", "build_hash": "abc1234", "uptime_secs": 120, "sessions": 2}
```

### `help`

Print a list of available commands with brief descriptions.

### `exit`

Close the control connection gracefully.

```
exit
```

Response:

```
goodbye
```

## Async Notifications

While connected, the daemon pushes async events as lines prefixed with `%`.
These can arrive at any time, including between a command and its response.

| Notification | Format | Description |
|---|---|---|
| Session created | `%session-changed <session_id>` | A new session was created |
| Session destroyed | `%session-closed <session_id>` | A session was destroyed |
| State changed | `%state-changed <old> <new>` | Daemon lifecycle state changed |
| Pane output | `%pane-output <pane_id>` | A pane produced output |

### Handling Notifications

Clients should check whether each line starts with `%` and is outside a
`%begin`/`%end` block. Lines matching `%begin <id>` open a response block;
lines matching `%end <id>` close it. Any `%`-prefixed line outside a block
is an async notification.

## Example Session

```
→ (connect and send handshake)
← %begin 0
← {"mode":"control","version":"0.1.0","build_hash":"abc1234-1700000000"}
← %end 0
→ new-session -n dev
← %begin 1
← {"session_id":1}
← %end 1
← %session-changed 1
→ list-panes 1
← %begin 2
← [{"pane_id":1,"cols":80,"rows":24}]
← %end 2
→ send-keys 1 echo hello
← %begin 3
← {"pane_id":1,"sent":true}
← %end 3
← %pane-output 1
→ capture-pane 1 -p
← %begin 4
← $ echo hello
← hello
← %end 4
→ exit
← %begin 5
← goodbye
← %end 5
```

## CLI Reference

```bash
# Print the full protocol reference
therminal-daemon --help-control

# Start a control-mode client session
therminal-daemon --control-mode
```
