# Streaming Pane Output

Therminal exposes a live PTY byte stream for each pane as an MCP resource:

```
terminal://pane/{pane_id}/output
```

This is the subscription-based streaming primitive — the equivalent of conductor-mcp's
`watch_pane` / `read_watch` / `stop_watch` trio.

## What it is

When output arrives from the PTY, the daemon broadcasts a `DaemonEvent::PaneOutput` event
internally. Any subscribed MCP client receives a `notifications/resources/updated`
notification for the pane's output URI. The client then calls `read_resource` to fetch the
updated content.

**What `read_resource` returns:** A snapshot of the pane's current visible grid — the same
plain-text lines that `terminal.panes.get_content` returns. The raw PTY bytes are not
forwarded verbatim; you get the rendered terminal state at the moment of the read.

**MIME type:** `text/plain`

## Subscription lifecycle

1. **Subscribe** — `resources/subscribe` with `uri = "terminal://pane/{id}/output"`.
   The daemon spawns a background forwarder task for this connection.

2. **Receive notifications** — Each time new PTY output arrives for the pane, the client
   gets a `notifications/resources/updated` with the same URI.

3. **Fetch content** — Call `resources/read` on the URI to get the current grid snapshot.

4. **Unsubscribe** — `resources/unsubscribe` cancels the background forwarder task.
   Re-subscribing to the same URI replaces the old subscription automatically.

Only `terminal://pane/{id}/output` supports subscriptions. `terminal://pane/{id}/content`
and `terminal://pane/{id}/scrollback` do not accept subscribe requests — the daemon returns
an error with a helpful hint pointing to the output URI.

## TypeScript example (`@modelcontextprotocol/sdk`)

```typescript
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const transport = new StdioClientTransport({
  command: "therminal",
  args: ["mcp"],
});

const client = new Client({ name: "my-client", version: "1.0.0" }, {
  capabilities: { experimental: {}, sampling: {} },
});
await client.connect(transport);

const PANE_ID = "abc123"; // from terminal.panes.list
const URI = `terminal://pane/${PANE_ID}/output`;

// 1. Subscribe
await client.subscribeResource({ uri: URI });

// 2. Attach notification handler
client.setNotificationHandler(
  "notifications/resources/updated",
  async (notification) => {
    if (notification.params.uri !== URI) return;

    // 3. Fetch current content on each notification
    const result = await client.readResource({ uri: URI });
    const text = result.contents[0]?.text ?? "";
    console.log("Pane output updated:\n", text);
  },
);

// ... do work ...

// 4. Unsubscribe when done
await client.unsubscribeResource({ uri: URI });
await client.close();
```

## Shell script example (raw JSON-RPC over the MCP socket)

The daemon's MCP socket lives at `$XDG_RUNTIME_DIR/therminal/mcp.sock` (Linux) or the path
returned by `therminal config show`. The `therminal mcp` subcommand bridges stdio to the
socket, so you can drive it with `nc` or `socat`:

```bash
PANE_ID="abc123"
URI="terminal://pane/${PANE_ID}/output"

# Subscribe and read one update, then unsubscribe
therminal mcp <<'EOF' | jq '.'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"shell","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"resources/subscribe","params":{"uri":"terminal://pane/abc123/output"}}
EOF
# Wait for a notifications/resources/updated message, then:
# {"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"terminal://pane/abc123/output"}}
# {"jsonrpc":"2.0","id":4,"method":"resources/unsubscribe","params":{"uri":"terminal://pane/abc123/output"}}
```

For a full interactive session, use `socat` or a small Python script to hold the connection
open and process the notification stream.

## When to use this vs `terminal.panes.get_content`

| Situation | Recommendation |
|-----------|----------------|
| Watching for a command to finish | Subscribe to `output`, poll `get_content` on notification |
| One-shot read of current screen state | `terminal.panes.get_content` |
| Waiting for a specific pattern | `terminal.panes.wait_for_output` (handles the subscribe loop internally) |
| Streaming output to another process in real time | Subscribe to `output` |
| Reading history above the visible grid | `terminal://pane/{id}/scrollback` (read-only, no subscription) |

`terminal.panes.wait_for_output` is the highest-level option — it wraps the subscribe/poll
cycle and returns when the pattern matches or a timeout expires. Use raw subscriptions when
you need to react to every screen change rather than wait for a specific condition.

## Comparison with conductor-mcp

| Feature | conductor-mcp | Therminal |
|---------|--------------|-----------|
| Subscribe | `watch_pane(pane_id)` | `resources/subscribe` on `terminal://pane/{id}/output` |
| Read buffered output | `read_watch(pane_id)` | `resources/read` on same URI (returns visible grid snapshot) |
| Stop watching | `stop_watch(pane_id)` | `resources/unsubscribe` on same URI |
| Transport | MCP tool calls | MCP resource subscription protocol (standard) |
| Output format | Raw PTY bytes (buffered since last read) | Rendered visible grid (plain text) |
| Pattern waiting | Manual loop | `terminal.panes.wait_for_output` handles it |
| Scrollback access | Via `read_watch` buffer | Separate `terminal://pane/{id}/scrollback` resource |

The key difference: conductor-mcp buffers raw bytes between `read_watch` calls, so each read
drains only new output. Therminal's `read_resource` always returns the current screen state
(a rendered grid snapshot), not a delta. If you need incremental output, compare successive
snapshots or use `terminal.panes.wait_for_output` with a regex.

## Resource listing

`terminal://pane/{id}/output` is always included in `list_resources` for every active pane,
alongside the `content` and `scrollback` resources. URI templates are also returned by
`list_resource_templates` if you need to construct URIs without listing first.
