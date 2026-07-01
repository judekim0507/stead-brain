# CLI Hooks

`pie` can run user-configured hooks when agent lifecycle events fire. Hooks are best-effort
side effects: they can run shell commands or POST JSON to webhooks, but they do not mutate agent
state and failures do not fail the agent turn.

## Configuration

User hooks live at:

```text
~/.pie/hooks.toml
```

Project hooks can live at:

```text
<repo>/.pie/hooks.toml
```

Project hooks are ignored by default because they can execute commands from a cloned repository.
Enable them explicitly from your user config:

```toml
allow_project_hooks = true
```

or for one process:

```sh
PIE_ALLOW_PROJECT_HOOKS=1 pie
```

## Examples

Append every finished tool call to a log:

```toml
[[hook]]
event = "tool_end"
command = "echo \"$PIE_TOOL_NAME error=$PIE_TOOL_IS_ERROR\" >> ~/.pie/tool-hooks.log"
timeout_ms = 3000
```

Run only when the `bash` tool finishes:

```toml
[[hook]]
event = "tool_end"
tool = "bash"
command = "echo \"bash finished in $PIE_SESSION_ID\" >> ~/.pie/bash-hooks.log"
```

Send a webhook when a turn ends:

```toml
[[hook]]
event = "turn_end"
webhook = "https://example.com/pie/hooks"
timeout_ms = 5000

[hook.headers]
Authorization = "Bearer your-token"
```

Send a desktop notification on macOS when the agent finishes a response:

```toml
[[hook]]
event = "agent_end"
command = "osascript -e 'display notification \"pie finished\" with title \"pie\"'"
```

Send a webhook when context compaction runs:

```toml
[[hook]]
event = "compaction"
webhook = "https://example.com/pie/compaction"
timeout_ms = 5000
```

## Hook Fields

Each `[[hook]]` supports:

```toml
event = "tool_end"       # required
command = "..."          # optional shell command
webhook = "https://..."  # optional HTTP POST endpoint
timeout_ms = 5000        # optional, default 5000
enabled = true           # optional, default true
cwd = "project"          # project | pie | home, default project
on_failure = "warn"      # warn | ignore, default warn
tool = "bash"            # optional filter for tool_* events
```

`command` and `webhook` can be used together; the command runs first, then the webhook is sent.

## Events

Supported events:

```text
agent_start
agent_end
turn_start
turn_end
message_start
message_update
message_end
tool_start
tool_update
tool_end
compaction
```

`message_update` can fire frequently while a model streams. Use it only when you actually need
streaming-level callbacks.

`compaction` fires after successful automatic context compaction and after manual `/compact`.
Its payload includes `compaction_trigger = "auto" | "manual"`, the estimated summarized token
count, and a truncated summary. Compaction summaries can contain sensitive context; only send
them to destinations you trust.

## Command Environment

Command hooks receive environment variables:

```text
PIE_HOOK_EVENT
PIE_HOOK_PAYLOAD
PIE_SESSION_ID
PIE_CWD
PIE_MODEL_PROVIDER
PIE_MODEL_ID
PIE_THINKING_LEVEL
PIE_MESSAGE_KIND
PIE_ASSISTANT_EVENT
PIE_TOOL_CALL_ID
PIE_TOOL_NAME
PIE_TOOL_IS_ERROR
PIE_COMPACTION_TRIGGER
PIE_COMPACTION_TOKENS_BEFORE
```

`PIE_HOOK_PAYLOAD` points to a temporary JSON file containing the same payload sent to webhooks.
Compaction summaries are available in this JSON payload, not as an environment variable.

## Webhook Payload

Webhook hooks receive `Content-Type: application/json` with fields such as:

```json
{
  "event": "tool_end",
  "session_id": "...",
  "cwd": "/path/to/repo",
  "model_provider": "openai",
  "model_id": "gpt-5.5",
  "thinking_level": "off",
  "source": "user",
  "tool_call_id": "call_...",
  "tool_name": "bash",
  "tool_is_error": false,
  "tool_result_summary": "..."
}
```

Long message and tool summaries are truncated before being placed in the payload.

A compaction webhook payload looks like:

```json
{
  "event": "compaction",
  "session_id": "...",
  "cwd": "/path/to/repo",
  "model_provider": "openai",
  "model_id": "gpt-5.5",
  "thinking_level": "off",
  "source": "user",
  "compaction_trigger": "auto",
  "compaction_tokens_before": 12345,
  "compaction_summary": "..."
}
```

Long compaction summaries are truncated before being placed in the payload.
