# Steadwright — rewrite plan and frozen contracts

Branch: `steadwright` in both `stead-brain` and `stead-macos`.

Goal: replace Stead's native perception/action layer and the QuickJS Playwright
shim with a Playwright-compatible control library written in Rust, driving
Chromium over the Chrome DevTools Protocol, using Playwright's own in-page
injected script for selectors, actionability, and ARIA snapshots. No Node, no
new process, no WebSocket in production.

Non-goals: matching Playwright's E2E test runner, Firefox/WebKit, `expect()`
assertions library, tracing, HAR, video.

## Why (one paragraph)

The current layer resolves locators in Rust against a half-megabyte AX dump,
fakes `locator.evaluate` in QuickJS, lowers link clicks to navigation, ships
untrusted `el.click()` by default, and needs ~4,300 tokens of prompt to explain
its divergences from Playwright. CDP already provides trusted input, real page
evaluation, history navigation, file upload, network events, and OOPIF access.
Playwright's injected script already provides the semantics. Wiring those
together from Rust removes the shim, most of the 300KB Chromium patch, and the
prompt.

## Architecture

```
model ── browser_exec({code}) ──▶ QuickJS (thin proxies only; every call → Rust)
                                     │
                                     ▼
                              steadwright (Rust)          crates/steadwright
                     Browser / Context / Page / Frame / Locator
                     actionability loop, waits, policy hooks, audit
                                     │
                                     ▼
                              steadwright-cdp (Rust)      crates/steadwright-cdp
                     CDP client: framing, ids, sessions, events
                     transports: fd-pair (prod) · WebSocket (dev/test)
                                     │
                                     ▼
                              Chromium browser process
                     DevToolsAgentHost::CreateForBrowser attached in
                     SteadBrainService, bridged to the brain's fd 3/4
                                     │
                                     ▼
                              Playwright injectedScript in an isolated
                              world per frame ("__steadwright")
```

Kept from the current Chromium patches: brain process launch, approvals UI,
drive overlay, tab ownership/takeover, Vault credential fill + taint, dialogs
and file-chooser handling that need browser UI. Everything in
`native-control-layer.patch` that snapshots the AX tree, dispatches AX actions,
or synthesizes input is deleted once steadwright is proven (Phase 6).

## Frozen contract 1 — CDP transport between browser and brain

Production transport is two extra pipes handed to the brain at launch, exactly
as Chromium's own `--remote-debugging-pipe` does for its child:

- fd 3: brain **reads** CDP messages from the browser.
- fd 4: brain **writes** CDP messages to the browser.
- Framing: ASCIIZ. Each message is one UTF-8 JSON document terminated by a
  single `\0` byte. No length prefix, no newline requirement.
- Message bodies are unmodified CDP JSON: `{"id","method","params","sessionId"}`
  from the brain, `{"id","result"|"error","sessionId"}` and
  `{"method","params","sessionId"}` from the browser.
- The browser side is a `content::DevToolsAgentHostClient` attached to
  `DevToolsAgentHost::CreateForBrowser(nullptr, {})`, owned by
  `SteadBrainService`, created when the helper is launched and detached when
  the helper exits. A blocking reader on the thread pool splits on `\0` and
  posts each message to the UI thread; writes go straight to fd 4.
- `MayAccessAllCookies()` returns true; `UsesBinaryProtocol()` false.
- The browser-side client may **inspect and veto** outgoing messages (policy
  hook, Phase 6). A vetoed command is answered with a CDP error object
  `{"id", "error": {"code": -32000, "message": "stead: <reason>"}}` from the
  bridge itself, never forwarded.
- The existing stdio JSON-lines brain protocol is untouched. CDP never rides on
  stdin/stdout.
- Dev/test transport: WebSocket to `ws://127.0.0.1:<port>/devtools/browser/<id>`
  from `--remote-debugging-port`. Same client code above the transport trait.

## Frozen contract 2 — steadwright public API shape (Rust)

Mirror Playwright's object model and names, snake_case, async:

- `Browser::connect(transport) -> Browser`; `browser.contexts()`,
  `browser.default_context()`, `browser.new_page()`, `browser.on_target(...)`.
- `Page`: `goto`, `reload`, `go_back`, `go_forward`, `url`, `title`, `content`,
  `evaluate`, `evaluate_handle`, `wait_for_load_state`, `wait_for_url`,
  `wait_for_selector`, `wait_for_function`, `wait_for_response`,
  `wait_for_request`, `wait_for_timeout`, `screenshot`, `set_viewport_size`,
  `bring_to_front`, `close`, `keyboard()`, `mouse()`, `main_frame()`,
  `frames()`, `frame_locator`, `locator`, `get_by_role`, `get_by_text`,
  `get_by_label`, `get_by_placeholder`, `get_by_alt_text`, `get_by_title`,
  `get_by_test_id`, `aria_snapshot`, `on_dialog`, `on_download`,
  `on_file_chooser`, `set_default_timeout`.
- `Frame`: same query/eval/wait surface scoped to the frame.
- `Locator`: `click`, `dblclick`, `hover`, `fill`, `type_text`, `press`,
  `clear`, `check`, `uncheck`, `set_checked`, `select_option`,
  `set_input_files`, `focus`, `blur`, `scroll_into_view_if_needed`,
  `drag_to`, `bounding_box`, `screenshot`, `inner_text`, `text_content`,
  `inner_html`, `input_value`, `get_attribute`, `is_visible`, `is_hidden`,
  `is_enabled`, `is_disabled`, `is_checked`, `is_editable`, `count`, `all`,
  `first`, `last`, `nth`, `filter`, `and_`, `or_`, `locator`, `get_by_*`,
  `frame_locator`, `content_frame`, `evaluate`, `evaluate_all`, `wait_for`,
  `aria_snapshot`, `highlight`.
- Selector strings are passed **verbatim** to the injected script's own
  parser. Rust never interprets selectors. `get_by_*` builds the same
  `internal:role=...` / `internal:text=...` selector strings Playwright's
  client builds.
- Strict mode is on by default for actions, exactly as Playwright: a locator
  matching more than one element fails with the Playwright error text.
- Errors are a single `Error` enum with a `message` that matches Playwright's
  wording where Playwright has wording (timeouts, strict violations,
  not-attached, detached frames).
- Every action takes an `ActionOptions { timeout, force, no_wait_after, trial }`
  subset matching Playwright; defaults: 30s timeout.
- All waits are event-driven or polled inside the page via the injected
  script (`waitForSelector`/`waitForFunction` poll in-page with rAF). No
  snapshot-fingerprint loops in Rust.

## Frozen contract 3 — model-facing `browser_exec`

- One tool. Description is one sentence: *Run Playwright JavaScript against
  the user's browser; `page` is the current tab, `context` the browser
  context, `state` persists across calls.*
- Globals: `page`, `context`, `browser`, `state`, `console`. Nothing else.
  No `help()`, no `display()`, no `page.snapshot()`, no `@eN` handles, no
  `page.info()`, no `page.use()`.
- Perception is `await page.ariaSnapshot()` (Playwright's `_snapshotForAI`
  shape with `[ref=eN]`), and refs resolve through `page.locator('aria-ref=eN')`
  exactly as Playwright MCP does. Static text and headings with levels are
  present because that is what ariaSnapshot emits.
- `locator.evaluate`, `evaluateAll`, `page.evaluate` run in the page.
- Tool result: `{result, logs}` plus image blocks for screenshots. No
  `operations`, no `last_observation`, no diffs. Result text capped at 32KB
  with a truncation notice.
- QuickJS stays as the sandbox. The bootstrap is only method-name proxies that
  forward `(receiver_id, method, args)` to Rust; it contains no semantics.

## Phases and gates

Each phase ends with gates I run myself. A phase is not done until its gate
passes on my machine.

- **P0 (Fable)** — quilt repair (done), branches (done), this doc, crate
  scaffolds, vendored injected script + license, fixture server for tests.
- **P1 (Sol)** — `steadwright-cdp`: transport trait, fd-pair and WebSocket
  transports, ASCIIZ framing, id/session correlation, typed `Command` helper,
  event subscription, auto-attach bookkeeping. Gate: unit tests + one live test
  attaching to headless Stead.app over WebSocket, `Target.getTargets` round
  trip, `Target.setAutoAttach{flatten:true}` receives `attachedToTarget`.
- **P2 (Sol)** — `steadwright` core: Browser/Context/Page/Frame lifecycle,
  target and frame tree tracking incl. OOPIF, isolated world per frame with
  injected script installed via `Page.addScriptToEvaluateOnNewDocument` +
  `Page.createIsolatedWorld`, `evaluate`, navigation + load-state waits,
  screenshot, viewport, keyboard/mouse dispatch. Gate: live tests against the
  fixture server (navigation, cross-origin iframe eval, screenshot bytes).
- **P3 (Sol)** — Locators and actions via injected script: querying, strict
  mode, actionability (visible, stable, enabled, receives events, hit-target
  check), click/fill/select/check/hover/press/type/upload, reads, waits,
  `aria_snapshot` with refs and `aria-ref=` resolution, dialogs, downloads,
  file chooser, request/response waits. Gate: fixture tests mirroring
  Playwright's semantics for every method listed in contract 2.
- **P4 (Sol, Chromium)** — fd 3/4 CDP bridge in `SteadBrainService` per
  contract 1, as a new patch `stead/brain/brain-cdp-bridge.patch`. Gate: I
  build `chrome`, relaunch, brain attaches over the fd pair, `Browser.getVersion`
  round trip logged. Runs in parallel with P2/P3.
- **P5 (Sol)** — brain integration: new `browser_exec` on steadwright with
  thin QuickJS proxies; delete `browser_repl.rs`, `BrowserPerceptionState`,
  `browser_tool_*` test-only code, `normalize_browser_exec_code`, the
  browser-automation skill, and every browser bullet in the system prompt
  except safety and credential rules. Gate: `cargo test --workspace`, manual
  run of the Apple configurator prompt from a cold chat, call count and wall
  clock recorded against the 50 / 315s baseline.
- **P6 (Sol, Chromium)** — delete the AX snapshot/action/input paths from
  `native-control-layer.patch`; keep credentials, taint, overlay, ownership,
  approvals; add the CDP policy veto hook (Input.* and Runtime.* on user-owned
  tabs go through the existing approval flow). Gate: build, relaunch, full
  benchmark again, patch applies to a pristine tree.
- **P7 (Fable)** — final review against ground truth, commit history, docs.

Order: P0 → P1 → (P2 ∥ P4) → P3 → P5 → P6 → P7.

## Test infrastructure (P0)

- `STEADWRIGHT_CHROMIUM` env var selects the binary; default search order:
  `stead-macos/build/src/out/Default/Stead.app/Contents/MacOS/Stead`, then
  `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`.
- Live tests launch it with `--headless=new --remote-debugging-port=0
  --user-data-dir=<tmp> --no-first-run about:blank`, parse the `DevTools
  listening on ws://...` line from stderr, and connect over WebSocket.
- Fixture pages are served by a tiny in-process HTTP server in
  `steadwright/tests/support` on two ports so cross-origin iframes are real.
- Live tests are `#[ignore]`-free but skip with a clear message when no
  Chromium is found, so `cargo test` stays green in CI without a browser.

## CDP crate decision

Hand-rolled `steadwright-cdp`. Reasons: the production transport is a custom
fd pair with ASCIIZ framing, which neither chromiumoxide (WebSocket-bound
transport) nor rustwright-core (single 62k-line file, pyo3 in the core crate,
alpha) supports without forking; the client itself is small (framing, id
correlation, `sessionId` routing, event fan-out). Protocol types stay
`serde_json::Value` with thin typed helpers for the ~40 commands steadwright
uses. `tokio` needs the `net` feature for `tokio::net::unix::pipe`; the dev
WebSocket transport uses `tokio-tungstenite` behind a default `ws` feature.

## Injected script contract (playwright-core 1.63.0)

Files: `crates/steadwright/vendor/playwright/{injectedScriptSource.js,
utilityScriptSource.js}`. Each source, when evaluated, populates a
`module.exports` object. Install per frame, per world, exactly as Playwright's
server does (coreBundle.js ~19940):

```js
(() => {
  const module = {};
  <injectedScriptSource>
  return new (module.exports.InjectedScript())(globalThis, {
    isUnderTest: false,
    sdkLanguage: "javascript",
    frameSeq: <u32 frame sequence assigned by steadwright, main frame = 0>,
    testIdAttributeName: "data-testid",
    stableRafCount: 1,          // Chromium
    browserName: "chromium",
    shouldPrependErrorPrefix: true,
    isUtilityWorld: true,
    customEngines: []
  });
})()
```

Evaluate that in the frame's isolated world (`Page.createIsolatedWorld`
with `worldName: "__steadwright"`, `grantUniveralAccess: true`) via
`Runtime.evaluate` and keep the returned `objectId` as the injected handle for
that frame + world. Re-install after every new document (track via
`Page.frameNavigated` / execution context creation events; use
`Page.addScriptToEvaluateOnNewDocument` with `worldName` to pre-create the
world). Element handles are `Runtime.RemoteObject.objectId` values; all calls
are `Runtime.callFunctionOn` with `functionDeclaration` of the form
`(injected, ...args) => injected.<method>(...)` and `objectId` of the
injected handle as `this`/first arg.

Entry points steadwright uses (method names on the InjectedScript instance):

- `parseSelector(selector: string) -> ParsedSelector` — selectors are passed
  verbatim as strings and parsed in-page. Rust never parses selectors.
- `querySelectorAll(parsed, root) -> Element[]`, `querySelector(parsed, root,
  strict) -> Element|undefined`. Strict-mode error text comes from
  `strictModeViolationError(parsed, matches)`.
- `elementState(node, state)` for `visible | hidden | enabled | disabled |
  editable | checked | unchecked | stable` → `{ matches: boolean, received }`.
  Actionability = poll these in-page with rAF until all required states match
  or the deadline passes (Playwright polls `elementState` from the server with
  backoff 1s/2s/4s/8s; do the same).
- `retarget(node, behavior)` with `"follow-label" | "no-follow-label" |
  "button-link"` to find the real control before acting.
- `expectHitTarget(hitPoint, targetElement)` → `"done"` or `{ hitTargetDescription }`,
  and `setupHitTargetInterceptor(node, action, hitPoint, blockAllEvents)` → a
  function that returns `"done"`/description after the input is dispatched.
  Click sequence = scroll into view (`DOM.scrollIntoViewIfNeeded`), compute
  the click point from `getElementBorderWidth` + `DOM.getContentQuads`,
  `expectHitTarget`, dispatch `Input.dispatchMouseEvent` mousePressed/Released,
  check interceptor result.
- `fill(node, value)` → `"needsinput"` (then dispatch `Input.insertText`) |
  `"needskeyboard"` | `"error:..."` | `"done"`. `selectOptions(node, options)`,
  `setInputFiles(node, payloads)` (use `DOM.setFileInputFiles` for real paths),
  `focusNode(node, resetSelectionIfNotFocused)`, `blurNode(node)`,
  `selectText(node)`, `dispatchEvent(node, type, eventInit)`.
- `ariaSnapshotJSON(node, { mode: "ai", depth?, boxes? })` →
  `{ json, iframeRefs, iframeDepths }`. Refs are `e<N>` in the main frame and
  `f<frameSeq>e<N>` in child frames. Cross-frame merge and YAML rendering
  happen in Rust: for each `iframeRefs` entry present in `iframeDepths`,
  resolve `aria-ref=<ref> >> internal:control=enter-frame >> body,frameset`,
  recurse, and splice the child array into the iframe node's `children`.
  YAML renderer is a direct port of `renderAriaSnapshotAsYaml`
  (coreBundle.js, ~90 lines, keys: role, quoted name, `[checked]`,
  `[disabled]`, `[expanded]`, `[active]`, `[level=N]`, `[pressed]`,
  `[selected]`, `[ref=eN]`, `[cursor=pointer]`, `[box=...]`, then `/url:` and
  `/placeholder:` props, then text/children). Keep byte-for-byte parity with
  Playwright's output; test against fixtures captured from Playwright.
- `aria-ref=e12` selectors resolve through the built-in `_createAriaRefEngine`
  against the last `ariaSnapshotJSON` of that frame. `f<seq>e<N>` refs must
  be routed to the frame with that sequence number before querying.
- `generateSelector(element, options)` for `locator.describe()`/errors.
- `previewNode(node)` for error messages, `createHighlight`/`hideHighlight`
  for `locator.highlight()`.
- `eval(expression)` and the UtilityScript `evaluate` helpers for
  `page.evaluate` argument serialization: port Playwright's `serializeAsCallArgument`
  / `parseEvaluationResultValue` (utilityScriptSource `UtilityScript.evaluate`)
  so `page.evaluate` supports Date/RegExp/Map/Set/BigInt/undefined round trips.

Not used: `expectArray`, recorder/console APIs, user overlays, screencast.

## Status — 2026-09-09

All phases landed on branch `steadwright` (stead-brain and stead-macos).

| gate | result |
|---|---|
| `cargo test --workspace` | 375 passed (steadwright 50 live, steadwright-cdp 5, brain-core 56, vendored pie 257) |
| steadwright live suite wall time | ~18 s |
| Chromium `stead_agent_control_service.cc` | 6,977 → 1,468 lines |
| brain `lib.rs` | 6,839 → 5,296 lines; `browser_repl.rs` (6,237) deleted |
| `stead-brain` release binary | 10.1 MB → 11.7 MB |
| end to end (headed Stead, Codex GPT-5.6-Sol) | "open example.com, title?" → 1 `browser_exec`, 5 s |
| Apple configurator, cold chat, Sol Medium | 3 `browser_exec` calls, 23 s to a correct stop: Apple's store was in "Be right back" maintenance (event day), so the configurator itself is unverified. Re-run when the store is up. Baseline was 50 calls / 315 s. |

Browser instruction in the prompt is now two bullets plus the safety and
credential rules; the browser-automation skill is gone.

Known follow-ups:
- Chat WebUI (`chrome://chat`) does not launch the brain until interaction;
  the new-tab page does. Pre-existing.
- `FillTotp` returns `credential_backend_unavailable` (pre-existing).
- CDP confirmations carry no chat session id in the audit log.
- Two vendored `pie-ai` clippy findings under Rust 1.96 (`redundant_guards`).
- Provider/effort selection is per browser profile; a fresh profile defaults to Claude.
- The two branding patches at the end of `series` remain unapplied by quilt
  (one is malformed) and need regenerating.

## Status — 2026-09-09, later: closing the gap with Aside

- Substrate timing on Apple's configurator (examples/timing.rs): ariaSnapshot 30–90 ms
  for 41 KB, count 24 ms; real Playwright fails the same label-covered radio clicks.
- Defaults: 5 s actions / 60 s navigation (Playwright MCP parity).
- `ariaSnapshot({interactive: true})` is ~71% smaller; `{diff: true}` returns a
  unified diff against the previous snapshot of that frame. The model adopts
  them unprompted beyond one prompt sentence.
- browser_exec output is plain text; optional `title` names the step; tool_status
  events carry the script and an output preview; the sidebar renders each
  execution as a code card (title, code, output) live and from history.
- CDP gate: Full access bypasses, Read only denies, Ask parks the command until
  the sidebar prompt is answered.
- Measured: "which sizes and chips" lookup on apple.com/ca in 14 s, one call.
  MacBook configuration at Sol Medium before the snapshot work: 20 calls / ~156 s;
  re-measure with interactive+diff snapshots.
