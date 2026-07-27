---
name: browser-automation
description: How to drive Stead's browser_exec REPL well — batching a task into one execution, waiting on state instead of sleeping, resolving locators unambiguously, and recovering from a failure without repeating it.
---

Read this before any non-trivial browser task. It is the operating contract for
`browser_exec`; the tool description only says what the tool is.

## One execution, not one step

`browser_exec` runs JavaScript in a persistent per-chat REPL. Each `await` is an
internal operation, not a step boundary. **Default to a single execution for the
whole task.** Before calling, encode every predictable observation, action,
wait, extraction, verification, and bounded alternative in the script. Use
results immediately in JavaScript and keep adapting in-process.

Return to the model only when the next move needs new reasoning, a visual
decision, or user input. Do not end an execution merely to look at intermediate
output — that is the single most expensive thing you can do, because every
round trip costs a full model turn.

`state` is the only thing that survives between executions. Lexical variables do
not.

## Wait on state, never on time

Stead implements Playwright's state-based waits natively. Each one runs to
completion inside a single operation and throws on timeout.

- `await page.waitForURL(pattern)` — string glob (`*` stops at `/`, `**` crosses
  it), RegExp, or a predicate receiving a URL object.
- `await page.waitForSelector(css, {state})` — `visible`/`attached` (default) or
  `hidden`/`detached`.
- `await locator.waitFor({state})` — same states, for a semantic locator.
- `await page.waitForFunction(() => ..., arg, {timeout, polling})` — the
  predicate runs in the page.
- `await page.waitForLoadState('load' | 'domcontentloaded' | 'networkidle')`.

**Register network waits before the action that triggers them**, exactly as in
Playwright:

```js
const wait = page.waitForResponse(/\/api\/orders/)
await page.getByRole('button', {name: 'Search'}).click()
const response = await wait   // {url, method, status, ok, resource_type}
```

The registration is captured before the click reaches the browser, so a response
that lands while the click is still settling cannot be missed.

Use `page.waitForTimeout(ms)` only for brief visual settling, and keep it under
2000ms. A fixed sleep is never the right way to wait for data.

**Never wait for an element you have not already seen.** A wait for something
that never appears costs its full 30s timeout and tells you nothing — it is the
single most expensive mistake available to you. To learn whether a control
exists, snapshot and look. Wait only to let a control you can already see become
enabled or actionable. If a wait is genuinely speculative, give it a short
explicit `{timeout: 3000}` so a wrong guess costs three seconds.

`page.goto()` already settles the page; do not chain `waitForLoadState` onto it.
And reserve `networkidle` for pages you know go quiet — store and marketing
pages with carousels, video, or analytics often never do, and it will burn the
whole timeout every time.

Note: Stead observes the network when a resource *completes*, so
`waitForRequest` resolves on completion too. It is not a dispatch-time
notification. When you need to act the moment a request is issued, watch for its
effect on the page instead.

## Perceive cheaply: interactive snapshots, handles, diffs

`await page.snapshot({interactive: true})` returns only the actionable elements
— buttons, links, radios, checkboxes, fields — each with an `@eN` handle:

```js
const snap = await page.snapshot({interactive: true})
const size = snap.elements.find((e) => e.role === 'radioButton' && e.name === '15-inch')
await page.locator('@' + size.ref).check()
```

Pass a handle straight back to `page.locator`. Handles re-resolve by role and
accessible name against a fresh tree immediately before acting, so they survive
the re-render your click just caused — you do not need to re-snapshot after
every action. Re-snapshot when you need elements that did not exist before, or
when a handle reports it is unknown.

After an action you receive a **diff**, not a fresh page: what was added, what
changed (with `from`/`to` for enabled and checked state), what was removed, and
a count of everything untouched. That diff is the answer to "what did my click
do?" — read it instead of taking another snapshot. `{"unchanged": true}` means
the action moved nothing, which is a signal worth acting on rather than
repeating the same click.

Plain `page.snapshot()` returns the entire accessibility tree. Reach for it only
when you genuinely need non-interactive structure.

## Resolve locators unambiguously

Actions auto-wait: a locator that matches nothing, or matches only disabled
nodes, is re-resolved until it becomes actionable. So do **not** add a snapshot
or sleep before a click just in case.

A bare locator that resolves to more than one element is an error, not a
coin flip. When that happens, the failure names what it matched — narrow with
`.filter({hasText})`, a more specific role or accessible name, or opt out
explicitly with `.first()`, `.nth(i)`, or `.last()`. Reach for `.first()` only
after you have confirmed the duplicates are legitimate.

Prefer semantic locators (`getByRole`, `getByLabel`, `getByText`) over CSS. When
the page structure is unknown, collect candidates once with `evaluateAll` or
`allTextContents`, decide in JavaScript, and continue in the same execution
rather than guessing selectors across executions.

## Recover by changing strategy, not by retrying

On failure, make one targeted observation and change approach materially. Do not
re-run a near-identical locator or command — it fails the same way and costs
another turn. Read the error: it distinguishes "matched nothing after waiting"
(check load state, active tab, overlays) from "resolved to N elements" (narrow
the locator).

If a signature is unclear, call `help('page')`, `help('locator')`, or
`help('page.waitForResponse')` **inside the execution**. It returns exact
signatures at runtime and costs no round trip. Never guess a signature.

## Treat an already-satisfied outcome as done

Before manipulating a control whose required value may already be set, perform
the smallest read that decides it. If it already matches, do not open its
editor or replay the interaction — continue to the remaining unsatisfied
outcomes. Words like "set", "select", or "ensure" describe a required final
state, unless the user explicitly asked for the transition itself.

## Choosing an interaction path

1. **Semantic** — `page.snapshot()` then role/label/text locators. The default
   for ordinary DOM pages.
2. **Visual** — `page.screenshot()` then `page.mouse` / `page.keyboard`. For
   canvas, virtualized editors, maps, and AX-poor surfaces. Before substantial
   editing, make a small write probe and verify it.
3. **Direct** — `locator.evaluateAll(fn)` for element collections,
   `page.evaluate(fn, arg)` for page-wide state.

Combine paths within one execution whenever the next input is already available
to the script.

## Verify before reporting

Prove the requested outcome from page state — a URL, a read-back value, a
visible confirmation. A click that did not throw is not evidence that it worked,
and a plausible partial result is not completion. When something is unmet, say
which part and why, rather than reporting overall success.
