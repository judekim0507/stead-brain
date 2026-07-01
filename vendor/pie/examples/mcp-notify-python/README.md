# mcp-notify-python — minimal MCP server that pushes notifications into pie

A self-contained Python example showing how to feed events into pie's trigger runtime via
the MCP server-push notification channel. Uses only the Python standard library; no MCP
SDK, no dependencies.

The server speaks JSON-RPC 2.0 over stdio (one message per line). It registers zero tools
and emits a `notifications/pie/demo/heartbeat` event every 10 seconds. Each heartbeat
carries a unique `_meta.pie_dedup_key` plus a human-readable `_meta.pie_summary`, so pie's
`McpNotificationHook` accepts each one as a distinct trigger and persists the summary into
the audit.

## Files

- `notify-server.py` — the MCP server. ~150 lines, stdlib only.
- `mcp.toml` — the pie config snippet that wires the server in.

## Run it with pie

1. Copy or merge the `[[server]]` block from `mcp.toml` into one of pie's MCP registries.
   The user-global location applies everywhere; the project-local location applies only
   when pie is launched from that repo.

   ```sh
   # user-global (works in every project):
   mkdir -p ~/.pie
   cp mcp.toml ~/.pie/mcp.toml   # or merge the [[server]] block into an existing file

   # OR project-local (only this repo):
   mkdir -p /path/to/your/project/.pie
   cp mcp.toml /path/to/your/project/.pie/mcp.toml
   ```

   The `args` path is relative to the cwd pie is launched from. Replace it with an
   absolute path to the `notify-server.py` if you want the config to work from anywhere.

2. Set a provider API key, then start pie:

   ```sh
   export OPENAI_API_KEY=sk-...   # or ANTHROPIC_API_KEY etc
   pie
   ```

   The startup banner should show `[mcp: connected to 1 server(s), 0 extra tool(s)]` and
   `[trigger sources: watching 1 configured MCP push source(s)]`. If the server failed to
   spawn, a diagnostic line is printed instead.

3. In the REPL, type `/triggers` to see the runtime status. Within ~12s the engine should
   show `accepted=1`, then `2`, `3`, … as heartbeats arrive. Each one is persisted to the
   session JSONL as a `Custom { customType: "trigger" }` entry.

## What the example demonstrates

| Concept | How it shows up |
| --- | --- |
| **JSON-RPC over stdio** | Server reads requests from stdin line-by-line, writes responses + notifications to stdout line-by-line. Logs go to stderr to keep the protocol channel clean. |
| **`initialize` handshake** | Server responds with `protocolVersion = "2025-03-26"`, empty capabilities, and a `serverInfo` block. pie's `pie_mcp::McpClient::initialize` requires this before `tools/list`. |
| **Server-push notifications** | `notifications/pie/demo/heartbeat` is emitted on a background thread every 10s. JSON-RPC notifications have no `id` field — that's how pie's read pump (`crates/mcp/src/client.rs`) routes them to the `take_notifications()` channel instead of the response router. |
| **Custom-method idempotency** | The MCP method is non-standard (not `tools/listChanged` etc), so the adapter requires an explicit `_meta.pie_dedup_key`. Without it the event is dropped. See `McpNotificationHook::idempotency_for` in `crates/coding-agent/src/triggers/mcp_notification_hook.rs`. |
| **Per-server key namespacing** | The runtime sees keys as `mcp:demo-notify:custom:heartbeat:<N>`, not the bare `heartbeat:<N>` the server emitted. This prevents collisions between servers and prevents user-supplied custom keys from colliding with built-in MCP method slots. |
| **Privacy contract** | The hook is hardcoded to `payload_visibility = Local`. The full params blob (including the illustrative `counter` / `ts` fields outside `_meta`) is dropped before persistence — only `payload_summary` survives into the audit. Opt in to per-event detail via `_meta.pie_summary`, which the server is declaring as safe to persist. |

## Make pie act on the events

By default, notifications are sunk and audited but no agent action runs. To get pie to
take action when a heartbeat arrives, install a dynamic trigger rule from chat:

```text
when mcp:demo-notify fires notifications/pie/demo/heartbeat, append the summary to /tmp/pie-heartbeats.log
```

pie's `NewTrigger` tool will persist the rule to a session sidecar, and each subsequent
heartbeat dispatches a sub-agent that inherits the parent harness config but starts with a
fresh conversation context.

## Make your own notification source

Swap the heartbeat body for whatever event source you want pie to react to: GitHub webhook
forwarder, MQTT bridge, file watcher, build-system finisher, etc. The contract is just:

1. Speak JSON-RPC 2.0 over stdio. Respond to `initialize` + `tools/list`.
2. Emit each event as a JSON-RPC notification (no `id`) on stdout, one per line.
3. For non-spec methods, include `_meta.pie_dedup_key` so the adapter accepts the event.
4. Include `_meta.pie_summary` only when the human-readable string is safe to persist —
   payload params themselves are dropped before audit.
