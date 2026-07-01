#!/usr/bin/env python3
# Minimal MCP server demonstrating server-push notifications into pie's trigger runtime.
# Speaks JSON-RPC 2.0 over stdio, line-delimited (one message per line).
#
# Requests handled:
#   - initialize           -> returns InitializeResult (protocolVersion 2025-03-26)
#   - tools/list           -> returns an empty tool list (this server has no tools)
#   - anything else        -> returns RPC error -32601 "Method not found"
#
# Notifications consumed (no response):
#   - notifications/initialized  (sent by pie after initialize)
#
# Notifications emitted (no `id`, server-pushed):
#   - notifications/pie/demo/heartbeat  every 10s, with `_meta.pie_dedup_key` (unique per
#     tick) and `_meta.pie_summary` (human-readable text).
#
# Wiring: see `.pie/mcp.toml` alongside this file. Run `pie` from the repo root, then
# `/triggers` in the REPL to see the heartbeat events show up in the audit.

import json
import sys
import threading
import time
from datetime import datetime, timezone

PROTOCOL_VERSION = "2025-03-26"
SERVER_NAME = "pie-demo-notify"
SERVER_VERSION = "0.1.0"
HEARTBEAT_INTERVAL_SECS = 10

_write_lock = threading.Lock()


def log(msg: str) -> None:
    # Logs MUST go to stderr — stdout is the JSON-RPC channel.
    print(f"[{SERVER_NAME}] {msg}", file=sys.stderr, flush=True)


def write_message(payload: dict) -> None:
    line = json.dumps(payload, separators=(",", ":"))
    with _write_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def send_response(request_id, result=None, error=None) -> None:
    msg = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result if result is not None else {}
    write_message(msg)


def send_notification(method: str, params: dict) -> None:
    write_message({"jsonrpc": "2.0", "method": method, "params": params})


def handle_request(method: str, params: dict, request_id) -> None:
    if method == "initialize":
        send_response(
            request_id,
            result={
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            },
        )
        log("initialize -> ok")
        return

    if method == "tools/list":
        send_response(request_id, result={"tools": []})
        return

    if method == "tools/call":
        send_response(
            request_id,
            error={"code": -32601, "message": "no tools available on this server"},
        )
        return

    send_response(
        request_id,
        error={"code": -32601, "message": f"method not found: {method}"},
    )


def handle_notification(method: str, params: dict) -> None:
    # The only inbound notification pie sends is `notifications/initialized` per MCP spec.
    if method == "notifications/initialized":
        log("client signaled initialized")
        return
    log(f"ignoring inbound notification: {method}")


def heartbeat_loop(stop_event: threading.Event) -> None:
    # Wait briefly so the initialize handshake settles before the first push.
    if stop_event.wait(2.0):
        return
    counter = 0
    while not stop_event.is_set():
        counter += 1
        now = datetime.now(timezone.utc).isoformat(timespec="seconds")
        send_notification(
            "notifications/pie/demo/heartbeat",
            {
                "_meta": {
                    # Unique per tick so pie does NOT dedup heartbeats together.
                    # The runtime will namespace this as
                    # `mcp:demo-notify:custom:heartbeat:<N>` (see McpNotificationHook).
                    "pie_dedup_key": f"heartbeat:{counter}",
                    # Opt-in human-readable summary that survives into the trigger audit.
                    # Without this, the persisted summary collapses to the method name only.
                    "pie_summary": f"heartbeat #{counter} at {now}",
                },
                # Extra params are dropped at the adapter (payload_visibility=Local) and
                # never echoed into the summary. Here for illustration only.
                "counter": counter,
                "ts": now,
            },
        )
        if stop_event.wait(HEARTBEAT_INTERVAL_SECS):
            return


def main() -> int:
    log(f"starting ({SERVER_NAME} v{SERVER_VERSION})")
    stop_event = threading.Event()
    heartbeat_thread = threading.Thread(
        target=heartbeat_loop, args=(stop_event,), daemon=True
    )
    heartbeat_thread.start()

    try:
        for raw_line in sys.stdin:
            line = raw_line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError as e:
                log(f"bad json: {e}")
                continue
            method = msg.get("method")
            params = msg.get("params") or {}
            request_id = msg.get("id")
            if method is None:
                continue
            if request_id is None:
                handle_notification(method, params)
            else:
                handle_request(method, params, request_id)
    except KeyboardInterrupt:
        pass
    finally:
        stop_event.set()
        log("shutting down")
    return 0


if __name__ == "__main__":
    sys.exit(main())
