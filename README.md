# Stead Brain

Bundled Rust helper process for Stead's agent brain.

This workspace is intentionally outside `helium-macos/` so Chromium consumes a
built helper artifact instead of owning Cargo dependency churn while the brain
is still being integrated.

## Layout

- `crates/stead-brain` — stdio helper binary shipped in `Stead.app`.
- `crates/stead-brain-core` — sessions, file roots, Pie adapters, browser-tool bridge.
- `crates/stead-brain-protocol` — newline-delimited JSON protocol shared with Chromium.
- `vendor/pie` — pinned `c4pt0r/pie` source copy.

## Build

```sh
cargo test --workspace
cargo build --release -p stead-brain
```

The release binary is `target/release/stead-brain`.

## Protocol

The helper speaks one JSON object per line over stdio. The browser owns process
lifecycle and translates tool calls to `AgentControl`; the helper never opens
Mojo directly.

## Current Status

Implemented:

- pinned `c4pt0r/pie` vendor dependency;
- newline-delimited JSON protocol types and golden tests;
- `stead-brain` stdio helper lifecycle (`initialize`, session calls, turns,
  tool results, shutdown);
- app-support session storage under `agents/main/sessions`;
- per-session `attachments/`, `tmp/`, and `artifacts/` directories;
- scoped file access with session-only default, explicit approved-root mode, and explicit full-disk mode;
- symlink-escape rejection and file/search/write caps;
- real Pie `AgentHarness` turns backed by `pie-ai` provider streaming;
- Pie provider support compiled for Anthropic, OpenAI Responses,
  OpenAI-compatible completions, OpenAI Codex Responses, Google, and Faux;
- macOS Keychain-backed provider credential store by default, with
  `agents/main/auth/provider_credentials.json` kept only as the explicit
  `STEAD_BRAIN_AUTH_STORE=file` dev/test fallback and legacy migration source;
- provider auth protocol calls for listing status, setting credentials, clearing
  credentials, importing Codex CLI auth, and starting OAuth;
- black-box `stead-brain` stdio tests for provider auth status, API-key save, and
  Codex auth import, with assertions that secrets are never echoed in helper
  events;
- Anthropic OAuth via Pie's PKCE helper, including token refresh before model calls;
- OpenAI Codex OAuth using the Codex-compatible PKCE endpoints and local callback,
  plus import from `$CODEX_HOME/auth.json` / `~/.codex/auth.json`;
- stream-time credential injection into `pie-ai`, including Anthropic OAuth bearer
  mode and Codex `chatgpt-account-id` propagation;
- explicit model resolution from Pie's model catalog, with no silent fake fallback
  for normal user turns;
- `list_models` protocol events backed by Pie's compiled model catalog, including
  provider auth capabilities for Claude, Codex, OpenAI, and Gemini;
- local `AGENTS.md` / `SOUL.md` files under `agents/main/` are created and
  merged into the run system prompt when populated;
- persistent non-secret memory under `agents/main/memory/`, exposed through the
  Pie-facing `memory` tool and injected into future run system prompts;
- bundled Pie-style Stead skills for credential handoff, Gmail, GitHub, Notion,
  and artifact creation are compiled into the helper;
- user Pie-style `SKILL.md` files under `agents/main/skills/` are loaded into
  the same native skill catalog and can override bundled skills by name, with a
  `Skill` invocation tool for on-demand markdown procedure loading;
- streamed assistant deltas, usage updates, tool statuses, and final stop events;
- `cancel_turn` aborts the active Pie harness for the target session;
- browser-mediated tool calls over stdio with pending `tool_result` resolution;
- Pie-facing browser tool catalog routed through a `BrowserToolBridge` trait;
- capped `browser.screenshot` PNGs become Pie image tool-result blocks, with
  base64 stripped from persisted tool details and oversized images reduced to
  metadata;
- Pie-facing scoped file tools (`files.list`, `files.read`, `files.search`,
  `files.write`);
- Pie-facing memory tool (`memory` with `save`, `list`, `read`, `search`, and
  `forget` actions), scoped to normalized memory keys rather than filesystem
  paths;
- Pie-facing local `get_time` tool for exact local/UTC timestamps in
  time-sensitive browser workflows;
- Pie-facing `ask_user` tool that pauses a turn for a scoped non-secret user
  decision and resumes through the browser-mediated `tool_result` path;
- Pie-facing `notification` tool for compact in-app milestones, completion
  notices, and blocked-state notices;
- Pie-facing local `WebFetch` tool for credentialless capped HTTP(S) reads, with
  no browser cookies or logged-in page state;
- packaging hook installs the release helper into
  `Stead.app/Contents/MacOS/stead-brain` before app signing.

The current chat session directory is always the working folder. Relative paths
resolve inside that folder, so normal scratch and output work should look like:

```json
{"path":"tmp/preview.html","content":"..."}
{"path":"artifacts/report.docx","content_base64":"..."}
```

Session file tools also accept explicit root aliases:

```json
{"root":"session_tmp","path":"preview.html","content":"..."}
{"root":"session_artifacts","path":"report.docx","content_base64":"..."}
```

When a file tool is installed for a chat turn, the current `session_id` is
supplied automatically for `session_*` roots. Passing an explicit `session_id`
is still supported for compatibility.

Default file access is `session_only`: no Downloads shortcut and no arbitrary
absolute paths. `approved_roots` are honored only when the helper is initialized
with `file_access_mode: "approved_roots"`. `file_access_mode: "full_disk"` is a
separate explicit mode; even then, relative paths still resolve against the
current chat session folder.

Deferred until Chromium wiring:

- Chromium build verification of the `BrainBroker` process launcher patch;
- runtime verification of browser-side `BrainBroker` tool routing through
  `AgentControl`.
