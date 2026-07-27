use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
    ToolExecutionMode,
};
use rquickjs::function::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Promise, async_with};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::{BrowserPerceptionState, BrowserToolBridge, browser_snapshot_fingerprint};

const MAX_BROWSER_EXEC_CODE_BYTES: usize = 64 * 1024;
const MAX_BROWSER_EXEC_OPERATIONS: usize = 128;
const BROWSER_EXEC_TIMEOUT: Duration = Duration::from_secs(120);
const BROWSER_REPL_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const BROWSER_REPL_STACK_LIMIT: usize = 1024 * 1024;
const MAX_CONSECUTIVE_BROWSER_EXEC_FAILURES: usize = 3;
const MAX_BROWSER_EXEC_LOG_BYTES: usize = 4 * 1024;
const MAX_BROWSER_EXEC_RESULT_BYTES: usize = 4 * 1024;
const MAX_COMPACT_OBSERVATION_NODES: usize = 32;
// Complex configurators routinely place their actual form controls after large
// global-nav and merchandising subtrees. Keep the model-facing observation
// compact, but let native locator resolution see the complete interactive page.
const MAX_BROWSER_REPL_SNAPSHOT_NODES: u64 = 2_000;
#[cfg(not(test))]
const NAVIGATION_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const NAVIGATION_SETTLE_TIMEOUT: Duration = Duration::from_millis(80);
// How long a committed navigation waits for the tree to stop growing before it
// reports the page. Client-rendered pages commit with a skeleton, so returning
// on the first URL match hands back a page whose controls have not been built
// yet; a page that never goes quiet must not cost more than this.
#[cfg(not(test))]
const NAVIGATION_CONTENT_GRACE: Duration = Duration::from_millis(1_200);
#[cfg(test)]
const NAVIGATION_CONTENT_GRACE: Duration = Duration::from_millis(20);
// Playwright's own defaults: 30s per wait, and a 100ms poll for the predicate
// waits. Every wait runs to completion inside a single native operation because
// MAX_BROWSER_EXEC_OPERATIONS caps one browser_exec call at 128 host calls — a
// JS-side poll would exhaust that budget long before a slow page settled.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_millis(400);
const MAX_WAIT_TIMEOUT: Duration = Duration::from_secs(120);
// How long an action operation keeps re-resolving a locator that currently
// matches nothing, mirroring Playwright's actionability auto-wait.
#[cfg(not(test))]
const ACTIONABILITY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const ACTIONABILITY_TIMEOUT: Duration = Duration::from_millis(500);

fn normalize_browser_exec_code(raw: &str) -> String {
    let mut code = raw.trim().to_string();
    // Some tool-capable model streams can accidentally append a rendered tool
    // invocation to an otherwise complete JavaScript body. Never evaluate that
    // transcript debris. The last statement terminator before the marker is a
    // conservative recovery boundary and preserves the valid program prefix.
    const TRANSCRIPT_MARKERS: &[&str] = &[
        "assistant to=functions.browser_exec",
        "assistant to=browser_exec",
        "<lemma to=functions.browser_exec",
        "[tool_call:browser_exec",
    ];
    if let Some(marker_index) = TRANSCRIPT_MARKERS
        .iter()
        .filter_map(|marker| code.find(marker))
        .min()
    {
        let prefix = &code[..marker_index];
        if let Some(statement_end) = prefix.rfind(';') {
            code.truncate(statement_end + 1);
        } else {
            code.truncate(marker_index);
        }
    }
    code.trim().to_string()
}

const PLAYWRIGHT_BOOTSTRAP: &str = r#"
(() => {
  if (globalThis.__steadPlaywrightInstalled) return;
  globalThis.__steadPlaywrightInstalled = true;
  globalThis.state = globalThis.state || {};
  globalThis.__steadLogs = [];

  // QuickJS can schedule several host calls at once (for example
  // Promise.all(locators.map(locator => locator.getAttribute('href')))). The
  // native browser bridge is intentionally single-flight, like Playwright's
  // protocol connection. Serialize calls here instead of racing and closing a
  // result channel under perfectly valid Playwright-style code.
  let nativeQueue = Promise.resolve();
  const nativeCall = (name, args = {}) => {
    const run = async () => {
      const envelope = JSON.parse(await globalThis.__stead_native_call(name, JSON.stringify(args)));
      if (!envelope.ok) {
        throw new Error(`${name}: ${envelope.error || 'Stead browser operation failed'}`);
      }
      return envelope.value;
    };
    const pending = nativeQueue.then(run, run);
    nativeQueue = pending.then(() => undefined, () => undefined);
    return pending;
  };

  const serializable = (value, seen = new Set()) => {
    if (value === undefined) return null;
    if (value === null || typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') return value;
    if (typeof value === 'bigint') return value.toString();
    if (typeof value === 'function') return `[Function ${value.name || 'anonymous'}]`;
    if (seen.has(value)) return '[Circular]';
    seen.add(value);
    if (Array.isArray(value)) return value.map((item) => serializable(item, seen));
    const out = {};
    for (const key of Object.keys(value)) {
      if (key.startsWith('_')) continue;
      try { out[key] = serializable(value[key], seen); } catch (_) {}
    }
    return out;
  };

  globalThis.console = {
    // A snapshot logs as its rendered tree, not as JSON of itself. Printing
    // `{"text":"- title: ...\n- link ..."}` would re-escape every newline and
    // cost more than the tree it is quoting.
    log: (...args) => globalThis.__steadLogs.push(args.map((v) => {
      if (typeof v === 'string') return v;
      if (v && typeof v === 'object' && typeof v.text === 'string' && v.interactive === true) return v.text;
      return JSON.stringify(serializable(v));
    }).join(' ')),
    info: (...args) => globalThis.console.log(...args),
    warn: (...args) => globalThis.console.log(...args),
    error: (...args) => globalThis.console.log(...args),
  };
  globalThis.display = (value) => value;
  globalThis.setTimeout = (callback, ms = 0, ...args) => {
    nativeCall('page.wait', { ms: Number(ms) || 0 }).then(() => callback(...args));
    return 0;
  };
  globalThis.clearTimeout = () => {};

  class SteadLocator {
    // `index === null` means the locator is strict: resolving it to more than
    // one element is an error rather than a silent pick. first()/nth()/last()
    // are how the model opts out, exactly as in Playwright.
    constructor(page, query, index = null, last = false) {
      this._page = page;
      this._query = query;
      this._index = index;
      this._last = last;
    }
    _args(extra = {}) {
      const args = { tab_id: this._page._tabId, query: this._query, last: this._last, ...extra };
      if (this._index !== null && this._index !== undefined) args.index = this._index;
      return args;
    }
    first() { return new SteadLocator(this._page, this._query, 0); }
    nth(index) { return new SteadLocator(this._page, this._query, index); }
    last() { return new SteadLocator(this._page, this._query, 0, true); }
    filter(options = {}) {
      let query = {...this._query};
      if (options.hasText !== undefined) {
        query = {...query, ...normalizeLocatorText(options.hasText)};
      }
      if (options.hasNotText !== undefined) {
        const negative = normalizeLocatorText(options.hasNotText);
        if (negative.name_regex !== undefined) query.name_not_regex = negative.name_regex;
        else query.name_not = negative.name;
      }
      return new SteadLocator(this._page, query, this._index, this._last);
    }
    async count() { return nativeCall('locator.count', this._args()); }
    async all() {
      const count = await this.count();
      return Array.from({length: count}, (_, index) => this.nth(index));
    }
    async allTextContents() { return nativeCall('locator.texts', this._args()); }
    async allInnerTexts() { return nativeCall('locator.texts', this._args()); }
    async textContent() { return nativeCall('locator.text', this._args()); }
    async innerText() { return this.textContent(); }
    async isVisible() { return (await this.count()) > (this._index ?? 0); }
    async isEnabled() { return nativeCall('locator.enabled', this._args()); }
    async isDisabled() { return !(await this.isEnabled()); }
    async isChecked() { return nativeCall('locator.checked', this._args()); }
    async inputValue() { return nativeCall('locator.value', this._args()); }
    async getAttribute(name) { return nativeCall('locator.attribute', this._args({ name: String(name) })); }
    async evaluateAll(fn, arg = undefined) {
      if (typeof fn !== 'function') throw new Error('evaluateAll requires a function');
      const descriptors = await nativeCall('locator.elements', this._args());
      const byId = new Map();
      const elements = descriptors.map((descriptor) => {
        const role = String(descriptor.role || '').toLowerCase();
        const id = String(descriptor.id || '');
        const tagName = role.includes('radio') || role.includes('checkbox') || role.includes('text')
          ? 'INPUT'
          : role === 'button' ? 'BUTTON' : role === 'link' ? 'A' : 'DIV';
        const type = role.includes('radio') ? 'radio' : role.includes('checkbox') ? 'checkbox' : role.includes('text') ? 'text' : '';
        const element = {
          id,
          tagName,
          type,
          name: '',
          value: descriptor.value ?? '',
          checked: descriptor.checked === true,
          disabled: descriptor.disabled === true,
          innerText: descriptor.name || '',
          textContent: descriptor.name || '',
          href: descriptor.href || '',
          htmlFor: '',
          outerHTML: `<${tagName.toLowerCase()}${type ? ` type="${type}"` : ''} aria-label="${String(descriptor.name || '').replaceAll('"', '&quot;')}">`,
          getAttribute(name) {
            const key = String(name).toLowerCase();
            if (key === 'aria-label' || key === 'title') return descriptor.name || null;
            if (key === 'role') return descriptor.role || null;
            if (key === 'disabled') return descriptor.disabled ? '' : null;
            if (key === 'checked' || key === 'aria-checked') return descriptor.checked == null ? null : String(descriptor.checked);
            if (key === 'type') return type || null;
            if (key === 'href') return descriptor.href || null;
            return null;
          },
        };
        byId.set(id, element);
        return element;
      });
      const previousDocument = globalThis.document;
      globalThis.document = {
        querySelectorAll(selector) {
          const match = String(selector).match(/^label\[for=["']?([^\]"']+)["']?\]$/);
          if (!match || !byId.has(match[1])) return [];
          const target = byId.get(match[1]);
          return [{ innerText: target.innerText, textContent: target.textContent, htmlFor: match[1] }];
        },
      };
      try { return await fn(elements, arg); }
      finally {
        if (previousDocument === undefined) delete globalThis.document;
        else globalThis.document = previousDocument;
      }
    }
    // Playwright's single-element counterpart to evaluateAll. Its absence cost
    // a round trip every time the model reached for the ordinary way to
    // inspect one control.
    async evaluate(fn, arg = undefined) {
      if (typeof fn !== 'function') throw new Error('evaluate requires a function');
      const index = this._index;
      const last = this._last;
      return this.evaluateAll((elements, value) => {
        if (elements.length === 0) throw new Error('locator.evaluate: locator resolved to no elements');
        if (elements.length > 1 && index === null && !last) {
          throw new Error(`locator.evaluate: locator resolved to ${elements.length} elements; narrow it or use first()/nth()/last()`);
        }
        const target = last ? elements[elements.length - 1] : elements[index ?? 0];
        if (!target) throw new Error('locator.evaluate: locator resolved to no elements');
        return fn(target, value);
      }, arg);
    }
    async boundingBox() { return nativeCall('locator.bounds', this._args()); }
    async click(options = {}) {
      return this._page._capture(await nativeCall('locator.click', this._args({ options })));
    }
    async dblclick(options = {}) { return nativeCall('locator.dblclick', this._args({ options })); }
    async hover(options = {}) { return nativeCall('locator.hover', this._args({ options })); }
    async fill(value, options = {}) { return nativeCall('locator.fill', this._args({ value: String(value), options })); }
    async type(value, options = {}) { return nativeCall('locator.type', this._args({ value: String(value), options })); }
    async clear(options = {}) { return nativeCall('locator.clear', this._args({ options })); }
    async check(options = {}) {
      return this._page._capture(await nativeCall('locator.check', this._args({ options })));
    }
    async uncheck(options = {}) {
      return this._page._capture(await nativeCall('locator.uncheck', this._args({ options })));
    }
    async focus() { return nativeCall('locator.focus', this._args()); }
    async scrollIntoViewIfNeeded() { return nativeCall('locator.scrollIntoView', this._args()); }
    async press(key) { return nativeCall('locator.press', this._args({ key: String(key) })); }
    async screenshot(options = {}) { return nativeCall('locator.screenshot', this._args({ options })); }
    async waitFor(options = {}) { return nativeCall('locator.waitFor', this._args({ options: this._page._withTimeout(options) })); }
    // Playwright's setInputFiles writes the file list straight onto the input.
    // Stead cannot: the renderer owns that value and forging it would bypass
    // the rules the element exists to enforce. So this arms the next chooser
    // and then activates the input, which produces a real upload through
    // Chromium's own file dialog path.
    async setInputFiles(files, options = {}) {
      const paths = (Array.isArray(files) ? files : [files])
        .filter((path) => path !== null && path !== undefined)
        .map((path) => String(path));
      await nativeCall('locator.setInputFiles', this._args({ paths, options }));
      return this._page._capture(await nativeCall('locator.click', this._args({ options })));
    }
    async dragTo(target, options = {}) {
      const from = await this.boundingBox();
      const to = await target.boundingBox();
      if (!from || !to) {
        throw new Error('dragTo needs bounds for both elements; take a screenshot and use page.mouse.drag with coordinates');
      }
      const center = (box) => ({ x: Math.round(box.x + box.width / 2), y: Math.round(box.y + box.height / 2) });
      return this._page._capture(await this._page.mouse.drag(center(from), center(to), options));
    }
  }

  class SteadPage {
    constructor(tabId = null, url = '') {
      this._tabId = Number.isInteger(tabId) && tabId > 0 ? tabId : null;
      this._url = typeof url === 'string' ? url : '';
      this._defaultTimeout = null;
      this.keyboard = {
        press: (key) => nativeCall('keyboard.press', { tab_id: this._tabId, key: String(key) }),
      };
      this.mouse = {
        move: (x, y) => nativeCall('mouse.move', { tab_id: this._tabId, x, y }),
        click: (x, y, options = {}) => nativeCall('mouse.click', { tab_id: this._tabId, x, y, options }),
        down: (options = {}) => nativeCall('mouse.down', { tab_id: this._tabId, options }),
        up: (options = {}) => nativeCall('mouse.up', { tab_id: this._tabId, options }),
        wheel: (dx, dy) => nativeCall('mouse.wheel', { tab_id: this._tabId, dx, dy }),
        drag: (from, to, options = {}) => nativeCall('mouse.drag', { tab_id: this._tabId, from, to, options }),
      };
    }
    async use(tabId) {
      this._tabId = Number(tabId);
      if (!Number.isInteger(this._tabId) || this._tabId <= 0) {
        this._tabId = null;
        throw new Error('tabId must be a positive Stead tab id');
      }
      await nativeCall('page.use', { tab_id: this._tabId });
      return this;
    }
    setDefaultTimeout(ms) {
      const timeout = Number(ms);
      this._defaultTimeout = Number.isFinite(timeout) && timeout >= 0 ? timeout : null;
    }
    // An explicit per-call timeout always wins; otherwise fall through to the
    // page default, and to the native default when none was set.
    _withTimeout(options = {}) {
      if (options.timeout !== undefined || this._defaultTimeout === null) return options;
      return { ...options, timeout: this._defaultTimeout };
    }
    _capture(result) {
      const observedUrl = result?.after?.snapshot?.url ?? result?.after?.url ??
        result?.snapshot?.url ?? result?.url;
      if (typeof observedUrl === 'string' && observedUrl) this._url = observedUrl;
      return result;
    }
    async goto(url) {
      const result = await nativeCall('page.goto', { tab_id: this._tabId, url: String(url) });
      const resolvedTabId = result && Number.isInteger(result.tab_id)
        ? result.tab_id
        : result && result.after && result.after.snapshot && Number.isInteger(result.after.snapshot.tab_id)
          ? result.after.snapshot.tab_id
          : null;
      if (resolvedTabId && resolvedTabId > 0) this._tabId = resolvedTabId;
      this._url = String(url);
      return this._capture(result);
    }
    async reload() { return nativeCall('page.reload', { tab_id: this._tabId }); }
    async close() { return nativeCall('page.close', { tab_id: this._tabId }); }
    async snapshot(options = {}) {
      const result = await nativeCall('page.snapshot', { tab_id: this._tabId, options });
      if (result && result.snapshot && typeof result.snapshot.url === 'string') this._url = result.snapshot.url;
      if (result && typeof result === 'object' && typeof result.slice !== 'function') {
        const stringify = () => JSON.stringify(result);
        Object.defineProperty(result, 'slice', {
          enumerable: false,
          value: (start, end) => stringify().slice(start, end),
        });
        Object.defineProperty(result, 'match', {
          enumerable: false,
          value: (pattern) => stringify().match(pattern),
        });
        Object.defineProperty(result, 'includes', {
          enumerable: false,
          value: (search, position) => stringify().includes(search, position),
        });
        Object.defineProperty(result, 'toString', {
          enumerable: false,
          value: stringify,
        });
      }
      return result;
    }
    async screenshot(options = {}) { return nativeCall('page.screenshot', { tab_id: this._tabId, options }); }
    async title() { return (await nativeCall('page.info', { tab_id: this._tabId })).title; }
    url() { return this._url; }
    async waitForTimeout(ms) { return nativeCall('page.wait', { ms }); }
    async waitForLoadState(state = 'load', options = {}) {
      return nativeCall('page.waitForLoadState', { tab_id: this._tabId, state: String(state), options: this._withTimeout(options) });
    }
    async waitForSelector(selector, options = {}) {
      const result = await nativeCall('page.waitForSelector', {
        tab_id: this._tabId,
        query: /^@?e\d+$/.test(String(selector))
          ? { kind: 'ref', ref: String(selector).replace(/^@/, '') }
          : { kind: 'css', selector: String(selector) },
        options: this._withTimeout(options),
      });
      return result.matches > 0 ? this.locator(String(selector)).first() : null;
    }
    async waitForFunction(fn, arg = undefined, options = {}) {
      const source = typeof fn === 'function'
        ? `(${fn.toString()})(${JSON.stringify(arg)})`
        : String(fn);
      return nativeCall('page.waitForFunction', { tab_id: this._tabId, source, options: this._withTimeout(options) });
    }
    // Playwright accepts a string glob, a RegExp, or a predicate. Globs and
    // RegExps match natively; a predicate has to run where the model wrote it,
    // so it drives page.waitForUrlChange and costs one operation per real
    // navigation rather than one per poll.
    async waitForURL(url, options = {}) {
      if (typeof url !== 'function') {
        const pattern = url instanceof RegExp
          ? { regex: url.source, flags: url.flags }
          : { glob: String(url) };
        // No JS-side default: the native side owns the timeout so one default
        // governs every wait.
        const result = await nativeCall('page.waitForURL', {
          tab_id: this._tabId,
          pattern,
          options: this._withTimeout(options),
        });
        this._url = result.url;
        return result.url;
      }
      const timeout = Number.isFinite(options.timeout) ? options.timeout : 30000;
      const deadline = Date.now() + timeout;
      let current = (await nativeCall('page.info', { tab_id: this._tabId })).url || this._url;
      for (;;) {
        this._url = current;
        if (url(parseHref(current))) return current;
        const remaining = deadline - Date.now();
        if (remaining <= 0) {
          throw new Error(`page.waitForURL timed out after ${timeout}ms; the page is still at ${current}`);
        }
        const changed = await nativeCall('page.waitForUrlChange', {
          tab_id: this._tabId,
          since: current,
          options: { timeout: remaining },
        });
        current = changed.url;
      }
    }
    async info() { return nativeCall('page.info', { tab_id: this._tabId }); }
    // Playwright's register-then-trigger shape:
    //   const wait = page.waitForResponse(/\/api\/orders/)
    //   await button.click()
    //   const response = await wait
    // The cursor call is issued synchronously here, before this function ever
    // awaits, so it enters the serialized native queue ahead of the click. That
    // is what makes "register before the action" actually hold.
    waitForResponse(urlOrPredicate, options = {}) { return this._waitForNetwork('response', urlOrPredicate, options); }
    waitForRequest(urlOrPredicate, options = {}) { return this._waitForNetwork('request', urlOrPredicate, options); }
    _waitForNetwork(kind, matcher, options = {}) {
      const registration = nativeCall('page.watchNetwork', { tab_id: this._tabId });
      const merged = this._withTimeout(options);
      const isPredicate = typeof matcher === 'function';
      const pattern = isPredicate
        ? null
        : matcher instanceof RegExp
          ? { regex: matcher.source, flags: matcher.flags }
          : { glob: String(matcher) };
      return (async () => {
        let cursor = (await registration).cursor ?? -1;
        if (!isPredicate) {
          const result = await nativeCall('page.pollNetwork', {
            tab_id: this._tabId, after_event_id: cursor, kind, pattern, options: merged,
          });
          return result.record;
        }
        const timeout = Number.isFinite(merged.timeout) ? merged.timeout : 30000;
        const deadline = Date.now() + timeout;
        for (;;) {
          const remaining = deadline - Date.now();
          if (remaining <= 0) {
            throw new Error(`page.waitFor${kind === 'request' ? 'Request' : 'Response'} timed out after ${timeout}ms`);
          }
          const batch = await nativeCall('page.pollNetwork', {
            tab_id: this._tabId, after_event_id: cursor, kind, pattern: null,
            options: { timeout: remaining },
          });
          cursor = batch.cursor ?? cursor;
          const hit = (batch.records || []).find((record) => matcher(record));
          if (hit) return hit;
        }
      })();
    }
    async evaluate(fn, arg = undefined) {
      const source = typeof fn === 'function'
        ? `(${fn.toString()})(${JSON.stringify(arg)})`
        : String(fn);
      return nativeCall('page.evaluate', { tab_id: this._tabId, source });
    }
    locator(selector) {
      const text = String(selector);
      // 'e12' is a ref printed on a snapshot line. Passing it straight back is
      // the cheapest way to act on something just observed. The '@' prefix is
      // accepted for code written against the older snapshot shape.
      if (/^@?e\d+$/.test(text)) {
        return new SteadLocator(this, { kind: 'ref', ref: text.replace(/^@/, '') });
      }
      return new SteadLocator(this, { kind: 'css', selector: text });
    }
    getByRole(role, options = {}) { return new SteadLocator(this, { kind: 'role', role: String(role), ...normalizeLocatorOptions(options) }); }
    getByText(text, options = {}) { return new SteadLocator(this, { kind: 'text', ...normalizeLocatorText(text, options) }); }
    getByLabel(text, options = {}) { return new SteadLocator(this, { kind: 'label', ...normalizeLocatorText(text, options) }); }
    getByPlaceholder(text, options = {}) { return new SteadLocator(this, { kind: 'placeholder', ...normalizeLocatorText(text, options) }); }
    getByTitle(text, options = {}) { return new SteadLocator(this, { kind: 'title', ...normalizeLocatorText(text, options) }); }
    getByAltText(text, options = {}) { return new SteadLocator(this, { kind: 'alt', ...normalizeLocatorText(text, options) }); }
    getByTestId(text, options = {}) { return new SteadLocator(this, { kind: 'testid', ...normalizeLocatorText(text, options) }); }
  }

  // QuickJS ships no URL/URLSearchParams. Playwright hands a URL to
  // waitForURL predicates and models reach for url.pathname and
  // url.searchParams, so provide the same shape rather than a bare string.
  const decodeComponent = (value) => {
    try { return decodeURIComponent(String(value).replace(/\+/g, ' ')); }
    catch (_) { return String(value); }
  };
  const searchParamsFrom = (search) => {
    const entries = [];
    for (const pair of String(search || '').replace(/^\?/, '').split('&')) {
      if (!pair) continue;
      const separator = pair.indexOf('=');
      entries.push(separator < 0
        ? [decodeComponent(pair), '']
        : [decodeComponent(pair.slice(0, separator)), decodeComponent(pair.slice(separator + 1))]);
    }
    return {
      get: (key) => { const hit = entries.find(([name]) => name === String(key)); return hit ? hit[1] : null; },
      getAll: (key) => entries.filter(([name]) => name === String(key)).map(([, value]) => value),
      has: (key) => entries.some(([name]) => name === String(key)),
      keys: () => entries.map(([name]) => name),
      values: () => entries.map(([, value]) => value),
      entries: () => entries.map((entry) => entry.slice()),
      forEach: (callback) => { for (const [name, value] of entries) callback(value, name); },
      toString: () => String(search || '').replace(/^\?/, ''),
    };
  };
  const parseHref = (href) => {
    const text = String(href ?? '');
    const match = /^([a-zA-Z][a-zA-Z0-9+.\-]*:)\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/.exec(text);
    if (!match) {
      return {
        href: text, protocol: '', username: '', password: '', host: '', hostname: '', port: '',
        pathname: text, search: '', hash: '', origin: '',
        searchParams: searchParamsFrom(''), toString: () => text,
      };
    }
    const protocol = match[1];
    const authority = match[2];
    const pathname = match[3] || '/';
    const search = match[4] || '';
    const hash = match[5] || '';
    const credentialsAt = authority.lastIndexOf('@');
    const credentials = credentialsAt < 0 ? '' : authority.slice(0, credentialsAt);
    const host = credentialsAt < 0 ? authority : authority.slice(credentialsAt + 1);
    const credentialsSplit = credentials.indexOf(':');
    // An IPv6 literal keeps its colons inside brackets, so only a colon after
    // the closing bracket delimits a port.
    const portAt = host.lastIndexOf(':');
    const bracketAt = host.lastIndexOf(']');
    const hasPort = portAt > bracketAt;
    return {
      href: text,
      protocol,
      username: credentialsSplit < 0 ? credentials : credentials.slice(0, credentialsSplit),
      password: credentialsSplit < 0 ? '' : credentials.slice(credentialsSplit + 1),
      host,
      hostname: hasPort ? host.slice(0, portAt) : host,
      port: hasPort ? host.slice(portAt + 1) : '',
      pathname,
      search,
      hash,
      origin: `${protocol}//${host}`,
      searchParams: searchParamsFrom(search),
      toString: () => text,
    };
  };
  globalThis.URL = globalThis.URL || function URL(href) { return parseHref(href); };

  const normalizeLocatorText = (text, options = {}) => {
    if (!(text instanceof RegExp)) return { name: String(text), ...options };
    return {
      ...options,
      name_regex: text.source,
      name_regex_flags: text.flags,
    };
  };
  const normalizeLocatorOptions = (options = {}) => {
    if (!(options.name instanceof RegExp)) return options;
    const { name, ...rest } = options;
    return { ...rest, ...normalizeLocatorText(name, rest) };
  };

  globalThis.SteadPage = SteadPage;
  globalThis.page = globalThis.page || new SteadPage();
  globalThis.context = {
    pages: async () => (await nativeCall('browser.pages')).map((tab) => new SteadPage(tab.tab_id, tab.url)),
    newPage: async (url = 'about:blank') => {
      const result = await nativeCall('browser.newPage', { url: String(url) });
      globalThis.page = new SteadPage(result.tab_id, result.url || String(url));
      return globalThis.page;
    },
  };
  globalThis.browser = globalThis.context;
  const locatorDescriptor = (locator) => {
    if (!(locator instanceof SteadLocator)) throw new Error('Expected a Stead locator');
    return { tab_id: locator._page._tabId, query: locator._query, index: locator._index };
  };
  globalThis.stead = {
    credentials: {
      list: async (origin = null, targetPage = globalThis.page) => {
        const result = await nativeCall('credentials.list', { tab_id: targetPage._tabId, origin });
        return Array.isArray(result) ? result : (result.credentials || []);
      },
      fill: (credential, usernameField, passwordField) => nativeCall('credentials.fill', {
        credential,
        username: locatorDescriptor(usernameField),
        password: locatorDescriptor(passwordField),
      }),
      fillTotp: (credential, field) => nativeCall('credentials.fillTotp', {
        credential,
        field: locatorDescriptor(field),
      }),
      markInjected: (targetPage = globalThis.page) => nativeCall('credentials.markInjected', { tab_id: targetPage._tabId }),
    },
    dialog: {
      accept: (handle, promptText = null) => nativeCall('dialog.handle', { handle: String(handle), accept: true, prompt_text: promptText }),
      dismiss: (handle) => nativeCall('dialog.handle', { handle: String(handle), accept: false }),
    },
    fileChooser: {
      setFiles: (handle, paths) => nativeCall('fileChooser.setFiles', { handle: String(handle), paths: Array.isArray(paths) ? paths : [paths] }),
    },
  };
  // Runtime signature reference. A wrong signature otherwise costs a whole
  // model round trip on a TypeError, which is the most avoidable turn there is.
  // Entries are [name, signature, description].
  const HELP_ENTRIES = [
    ['page.goto', 'page.goto(url) => Promise<observation>', 'Navigate the current tab. Opens an agent-owned tab when none is attached.'],
    ['page.reload', 'page.reload() => Promise<observation>', 'Reload the current page.'],
    ['page.close', 'page.close() => Promise<void>', 'Close the current tab.'],
    ['page.url', 'page.url() => string', 'Last observed URL. Synchronous, like Playwright; call page.info() for a fresh read.'],
    ['page.title', 'page.title() => Promise<string>', 'Current page title.'],
    ['page.info', 'page.info() => Promise<{tab_id, url, title}>', 'Fresh URL and title straight from the browser.'],
    ['page.snapshot', 'page.snapshot(options?) => Promise<{text, diff, elements, url, title}>', 'Accessibility snapshot. text is the indented tree; diff is a unified diff against the previous snapshot of this tab; elements is the same nodes as objects for filtering in the REPL. Pass {interactive:false} for the raw tree.'],
    ['page.screenshot', 'page.screenshot(options?) => Promise<image>', 'PNG of the current tab, attached to the tool result.'],
    ['page.evaluate', 'page.evaluate(fn, arg?) => Promise<any>', 'Run a function in the page and return its value. document/window are the page\'s.'],
    ['page.waitForTimeout', 'page.waitForTimeout(ms) => Promise<void>', 'Fixed sleep. Prefer a state-based wait; use only for brief visual settling.'],
    ['page.waitForLoadState', "page.waitForLoadState(state?, options?) => Promise<{state}>", "state: 'load' (default) | 'domcontentloaded' | 'networkidle'. Throws on timeout."],
    ['page.waitForURL', 'page.waitForURL(urlOrRegExpOrPredicate, options?) => Promise<string>', 'Wait until the URL matches. A string is a glob (* stops at /, ** crosses it). A predicate receives a URL object.'],
    ['page.waitForSelector', 'page.waitForSelector(selector, options?) => Promise<Locator|null>', "options.state: 'visible'|'attached' (default visible) | 'hidden'|'detached'. Returns null for absence states."],
    ['page.waitForFunction', 'page.waitForFunction(fn, arg?, options?) => Promise<any>', 'Poll a predicate in the page until truthy. options: {timeout, polling}.'],
    ['page.waitForResponse', 'page.waitForResponse(urlOrPredicate, options?) => Promise<Response>', 'Register BEFORE the action that triggers it. Response: {url, status, ok, method, resourceType}.'],
    ['page.waitForRequest', 'page.waitForRequest(urlOrPredicate, options?) => Promise<Request>', 'Register BEFORE the action that triggers it. Request: {url, method, resourceType}.'],
    ['page.setDefaultTimeout', 'page.setDefaultTimeout(ms) => void', 'Default timeout for this page\'s waits and actions.'],
    ['page.locator', 'page.locator(cssOrRef) => Locator', "CSS selector, or an 'eN' ref straight out of a snapshot line. Refs re-resolve by role and name, so they survive the re-render your last click caused."],
    ['page.getByRole', 'page.getByRole(role, options?) => Locator', 'options: {name (string|RegExp), exact, checked, group}.'],
    ['page.getByText', 'page.getByText(text, options?) => Locator', 'options: {exact}.'],
    ['page.getByLabel', 'page.getByLabel(text, options?) => Locator', 'Form control by label.'],
    ['page.getByPlaceholder', 'page.getByPlaceholder(text, options?) => Locator', 'Input by placeholder.'],
    ['page.getByTitle', 'page.getByTitle(text, options?) => Locator', 'By title attribute.'],
    ['page.getByAltText', 'page.getByAltText(text, options?) => Locator', 'By image alt text.'],
    ['page.getByTestId', 'page.getByTestId(id, options?) => Locator', 'By test id.'],
    ['page.keyboard', 'page.keyboard.press(key) => Promise<observation>', "Key combos use '+', e.g. 'Control+A'."],
    ['page.mouse', 'page.mouse.{move,click,down,up,wheel,drag}(...) => Promise<observation>', 'Coordinate input for canvas and AX-poor surfaces.'],
    ['locator.first', 'locator.first() / .nth(i) / .last() => Locator', 'Opt out of strict mode. A bare locator matching several elements throws.'],
    ['locator.filter', 'locator.filter(options) => Locator', 'options: {hasText, hasNotText}; strings or RegExp.'],
    ['locator.count', 'locator.count() => Promise<number>', 'Number of matches. Never strict.'],
    ['locator.all', 'locator.all() => Promise<Locator[]>', 'One locator per match.'],
    ['locator.click', 'locator.click(options?) => Promise<observation>', 'Auto-waits for the element to exist and become enabled.'],
    ['locator.dblclick', 'locator.dblclick(options?) => Promise<observation>', 'Double click.'],
    ['locator.hover', 'locator.hover(options?) => Promise<observation>', 'Move the pointer over the element.'],
    ['locator.dragTo', 'locator.dragTo(target, options?) => Promise<observation>', 'Drag this element onto another locator.'],
    ['locator.fill', 'locator.fill(value, options?) => Promise<observation>', 'Set the value of an input in one step.'],
    ['locator.type', 'locator.type(value, options?) => Promise<observation>', 'Type character by character.'],
    ['locator.clear', 'locator.clear(options?) => Promise<observation>', 'Clear an input.'],
    ['locator.check', 'locator.check(options?) => Promise<observation>', 'Check a checkbox or radio and verify the result.'],
    ['locator.uncheck', 'locator.uncheck(options?) => Promise<observation>', 'Uncheck a checkbox.'],
    ['locator.press', 'locator.press(key) => Promise<observation>', 'Press a key with the element focused.'],
    ['locator.setInputFiles', 'locator.setInputFiles(paths, options?) => Promise<observation>', 'Upload files through an <input type=file>. Paths must be absolute. Arms the next file chooser, then activates the input; pass [] to cancel instead.'],
    ['locator.focus', 'locator.focus() => Promise<void>', 'Focus the element.'],
    ['locator.scrollIntoViewIfNeeded', 'locator.scrollIntoViewIfNeeded() => Promise<void>', 'Scroll the element into view.'],
    ['locator.waitFor', 'locator.waitFor(options?) => Promise<{state, matches}>', "options.state: 'visible'|'attached'|'hidden'|'detached'. Throws on timeout."],
    ['locator.textContent', 'locator.textContent() => Promise<string>', 'Accessible name of the match. innerText() is the same value in Stead — the source is the accessibility tree, not rendered DOM text.'],
    ['locator.allTextContents', 'locator.allTextContents() => Promise<string[]>', 'Accessible names of every match. allInnerTexts() is the same value.'],
    ['locator.getAttribute', 'locator.getAttribute(name) => Promise<string|null>', 'href, aria-label, id, class, name, type, role are resolved natively.'],
    ['locator.inputValue', 'locator.inputValue() => Promise<string|null>', 'Current value of an input.'],
    ['locator.isChecked', 'locator.isChecked() => Promise<boolean|null>', 'Checked state.'],
    ['locator.isEnabled', 'locator.isEnabled() / isDisabled() => Promise<boolean>', 'Enabled state.'],
    ['locator.isVisible', 'locator.isVisible() => Promise<boolean>', 'Whether the locator resolves. The AX tree already excludes non-exposed nodes.'],
    ['locator.boundingBox', 'locator.boundingBox() => Promise<rect|null>', 'Viewport rect, for coordinate work.'],
    ['locator.evaluate', 'locator.evaluate(fn, arg?) => Promise<any>', 'Run a function over the single match as an element-like descriptor. Strict: narrow the locator or use first()/nth()/last() when it resolves to several.'],
    ['locator.evaluateAll', 'locator.evaluateAll(fn, arg?) => Promise<any>', 'Run a function over every match as element-like descriptors.'],
    ['locator.screenshot', 'locator.screenshot(options?) => Promise<image>', 'PNG of one element.'],
    ['context.pages', 'context.pages() => Promise<Page[]>', 'All open tabs. Async — await it.'],
    ['context.newPage', 'context.newPage(url?) => Promise<Page>', 'Open an agent-owned tab and make it the default page.'],
    ['stead.credentials', 'stead.credentials.{list,fill,fillTotp,markInjected}(...)', 'Fill saved credentials without exposing secrets to the model.'],
    ['stead.dialog', 'stead.dialog.{accept,dismiss}(handle, promptText?)', 'Handle a JavaScript dialog reported by an observation.'],
    ['stead.fileChooser', 'stead.fileChooser.setFiles(handle, paths)', 'Answer a file chooser reported by an observation.'],
  ];
  globalThis.help = (name) => {
    if (name === undefined || name === null || name === '') {
      const topics = [...new Set(HELP_ENTRIES.map(([entry]) => entry.split('.')[0]))];
      return `help(name) — topics: ${topics.join(', ')}\n` +
        `Pass a prefix ("page") or an exact name ("page.waitForURL").`;
    }
    const needle = String(name);
    const exact = HELP_ENTRIES.filter(([entry]) => entry === needle);
    const matches = exact.length > 0
      ? exact
      : HELP_ENTRIES.filter(([entry]) => entry.startsWith(`${needle}.`) || entry === needle);
    if (matches.length === 0) {
      const known = HELP_ENTRIES
        .filter(([entry]) => entry.toLowerCase().includes(needle.toLowerCase()))
        .map(([entry]) => entry);
      return known.length > 0
        ? `No exact entry for ${needle}. Did you mean: ${known.join(', ')}?`
        : `No entry for ${needle}. Call help() for the topic list.`;
    }
    return matches
      .map(([, signature, description]) => `${signature}\n    ${description}`)
      .join('\n');
  };

  globalThis.__steadSerializable = serializable;
})();
"#;

#[derive(Default)]
pub(crate) struct BrowserRuntimePool {
    runtimes: Mutex<HashMap<String, Arc<BrowserJsRuntime>>>,
}

impl BrowserRuntimePool {
    async fn runtime_for(&self, session_id: &str) -> Result<Arc<BrowserJsRuntime>, String> {
        if let Some(runtime) = self.runtimes.lock().await.get(session_id).cloned() {
            return Ok(runtime);
        }
        let runtime = Arc::new(BrowserJsRuntime::new().await?);
        let mut runtimes = self.runtimes.lock().await;
        Ok(runtimes
            .entry(session_id.to_string())
            .or_insert_with(|| runtime.clone())
            .clone())
    }
}

struct BrowserJsRuntime {
    runtime: AsyncRuntime,
    context: AsyncContext,
    execution_lock: Mutex<()>,
}

impl BrowserJsRuntime {
    async fn new() -> Result<Self, String> {
        let runtime = AsyncRuntime::new().map_err(|error| error.to_string())?;
        runtime.set_memory_limit(BROWSER_REPL_MEMORY_LIMIT).await;
        runtime.set_max_stack_size(BROWSER_REPL_STACK_LIMIT).await;
        let context = AsyncContext::full(&runtime)
            .await
            .map_err(|error| error.to_string())?;
        async_with!(context => |ctx| {
            ctx.eval::<(), _>(PLAYWRIGHT_BOOTSTRAP)
                .catch(&ctx)
                .map_err(|error| format!("failed to initialize Steadwright: {error:?}"))
        })
        .await?;
        Ok(Self {
            runtime,
            context,
            execution_lock: Mutex::new(()),
        })
    }

    async fn execute(
        &self,
        code: &str,
        default_tab_id: Option<i32>,
        bridge: Arc<dyn BrowserToolBridge>,
        perception: Arc<BrowserPerceptionState>,
        tool_call_id: &str,
        cancel: CancellationToken,
    ) -> Result<BrowserExecOutcome, String> {
        let _guard = self.execution_lock.lock().await;
        let execution_cancel = cancel.child_token();
        let watchdog_cancel = execution_cancel.clone();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(BROWSER_EXEC_TIMEOUT).await;
            watchdog_cancel.cancel();
        });
        let interrupt_cancel = execution_cancel.clone();
        self.runtime
            .set_interrupt_handler(Some(Box::new(move || interrupt_cancel.is_cancelled())))
            .await;

        let host = Arc::new(BrowserExecutionHost::new(
            bridge,
            perception,
            tool_call_id,
            default_tab_id,
            execution_cancel.clone(),
        ));
        let host_for_js = host.clone();
        let host_fn = move |name: String, args_json: String| {
            let host = host_for_js.clone();
            async move {
                let envelope = match serde_json::from_str::<Value>(&args_json) {
                    Ok(args) => match host.dispatch(&name, args).await {
                        Ok(value) => json!({ "ok": true, "value": value }),
                        Err(error) => json!({ "ok": false, "error": error }),
                    },
                    Err(error) => {
                        json!({ "ok": false, "error": format!("invalid operation arguments: {error}") })
                    }
                };
                Ok::<String, rquickjs::Error>(envelope.to_string())
            }
        };

        let code = code.to_string();
        let tab_setup = default_tab_id
            .filter(|tab_id| *tab_id > 0)
            .map(|tab_id| {
                format!(
                    "if (!Number.isInteger(globalThis.page._tabId) || globalThis.page._tabId <= 0) globalThis.page._tabId = {tab_id};"
                )
            })
            .unwrap_or_default();
        let wrapped = format!(
            r#"(async () => {{
                globalThis.__steadLogs = [];
                {tab_setup}
                try {{
                    const value = await (async () => {{
                        {code}
                    }})();
                    return JSON.stringify({{ ok: true, value: globalThis.__steadSerializable(value), logs: globalThis.__steadLogs }});
                }} catch (error) {{
                    return JSON.stringify({{
                        ok: false,
                        error: String(error && error.message ? error.message : error),
                        stack: error && error.stack ? String(error.stack) : null,
                        logs: globalThis.__steadLogs,
                    }});
                }}
            }})()"#
        );

        let evaluated = async_with!(self.context => |ctx| {
            ctx.globals()
                .set("__stead_native_call", Func::from(Async(host_fn)))
                .catch(&ctx)
                .map_err(|error| format!("failed to bind Steadwright host: {error:?}"))?;
            let promise = ctx
                .eval::<Promise, _>(wrapped)
                .catch(&ctx)
                .map_err(|error| format!("Steadwright JavaScript error: {error:?}"))?;
            promise
                .into_future::<String>()
                .await
                .catch(&ctx)
                .map_err(|error| format!("Steadwright promise failed: {error:?}"))
        })
        .await;

        watchdog.abort();
        self.runtime.set_interrupt_handler(None).await;
        if execution_cancel.is_cancelled() && evaluated.is_err() {
            return Err(if cancel.is_cancelled() {
                "browser execution cancelled".to_string()
            } else {
                "browser execution timed out".to_string()
            });
        }
        let encoded = evaluated?;
        let result: Value = serde_json::from_str(&encoded)
            .map_err(|error| format!("invalid Steadwright result: {error}"))?;
        let outcome = host.outcome(result.clone());
        if result.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("browser execution failed")
                .to_string());
        }
        Ok(outcome)
    }
}

#[derive(Clone)]
struct CapturedImage {
    data: String,
    mime_type: String,
}

struct BrowserExecOutcome {
    result: Value,
    operations: Vec<Value>,
    last_observation: Option<Value>,
    images: Vec<CapturedImage>,
}

struct BrowserExecutionHost {
    bridge: Arc<dyn BrowserToolBridge>,
    perception: Arc<BrowserPerceptionState>,
    parent_tool_call_id: String,
    default_tab_id: StdMutex<Option<i32>>,
    operation_count: AtomicUsize,
    operations: StdMutex<Vec<Value>>,
    last_observation: StdMutex<Option<Value>>,
    images: StdMutex<Vec<CapturedImage>>,
    mouse_position: StdMutex<(i64, i64)>,
    cancel: CancellationToken,
}

impl BrowserExecutionHost {
    fn new(
        bridge: Arc<dyn BrowserToolBridge>,
        perception: Arc<BrowserPerceptionState>,
        parent_tool_call_id: &str,
        default_tab_id: Option<i32>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            bridge,
            perception,
            parent_tool_call_id: parent_tool_call_id.to_string(),
            default_tab_id: StdMutex::new(default_tab_id.filter(|tab_id| *tab_id > 0)),
            operation_count: AtomicUsize::new(0),
            operations: StdMutex::new(Vec::new()),
            last_observation: StdMutex::new(None),
            images: StdMutex::new(Vec::new()),
            mouse_position: StdMutex::new((0, 0)),
            cancel,
        }
    }

    fn outcome(&self, result: Value) -> BrowserExecOutcome {
        BrowserExecOutcome {
            result,
            operations: self
                .operations
                .lock()
                .expect("operations mutex poisoned")
                .clone(),
            last_observation: self
                .last_observation
                .lock()
                .expect("observation mutex poisoned")
                .clone(),
            images: self.images.lock().expect("images mutex poisoned").clone(),
        }
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, String> {
        if self.cancel.is_cancelled() {
            return Err("browser execution cancelled".to_string());
        }
        let index = self.operation_count.fetch_add(1, Ordering::Relaxed);
        if index >= MAX_BROWSER_EXEC_OPERATIONS {
            return Err(format!(
                "browser execution exceeded the {MAX_BROWSER_EXEC_OPERATIONS}-operation safety limit"
            ));
        }
        match name {
            "browser.pages" => self.list_tabs().await,
            "browser.newPage" => {
                let url = required_string(&args, "url")?;
                self.open_agent_page(&url).await
            }
            "page.use" => {
                let tab_id = required_positive_i32(&args, "tab_id")?;
                *self.default_tab_id.lock().expect("tab mutex poisoned") = Some(tab_id);
                Ok(json!({ "tab_id": tab_id }))
            }
            "page.goto" => {
                let url = required_string(&args, "url")?;
                let Some(tab_id) = self.current_tab_id(&args) else {
                    return self.open_agent_page_and_observe(&url).await;
                };
                let baseline = self.snapshot(tab_id, false, false).await.ok();
                let action = match self
                    .call_native("browser.navigate", json!({ "tab_id": tab_id, "url": url }))
                    .await
                {
                    Ok(action) => action,
                    Err(error) if is_missing_tab_error(&error) => {
                        *self.default_tab_id.lock().expect("tab mutex poisoned") = None;
                        return self.open_agent_page_and_observe(&url).await;
                    }
                    Err(error) => return Err(error),
                };
                self.observe_navigation(tab_id, &url, baseline, action)
                    .await
            }
            "page.reload" => {
                let tab_id = self.tab_id(&args).await?;
                let snapshot = self.snapshot(tab_id, false, false).await?;
                let url = snapshot
                    .pointer("/snapshot/url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "current page URL is unavailable".to_string())?;
                let action = self
                    .call_native("browser.navigate", json!({ "tab_id": tab_id, "url": url }))
                    .await?;
                self.observe_after_action(tab_id, Some(snapshot), action, false)
                    .await
            }
            "page.close" => {
                let tab_id = self.tab_id(&args).await?;
                self.call_native("browser.close_tab", json!({ "tab_id": tab_id }))
                    .await
            }
            "page.snapshot" => {
                let tab_id = self.tab_id(&args).await?;
                let options = args.get("options").cloned().unwrap_or_else(|| json!({}));
                let include_bounds = options
                    .get("includeBounds")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let include_values = options
                    .get("includeValues")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let result = self
                    .snapshot(tab_id, include_bounds, include_values)
                    .await?;
                self.remember_observation(&result);
                // Interactive is the shape a page snapshot should have had all
                // along; the raw tree is the special case, behind an opt-out.
                if options
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
                {
                    return Ok(self.interactive_snapshot(tab_id, &result));
                }
                Ok(result)
            }
            "page.info" => {
                let tab_id = self.tab_id(&args).await?;
                let snapshot = self.snapshot(tab_id, false, false).await?;
                Ok(json!({
                    "tab_id": tab_id,
                    "url": snapshot.pointer("/snapshot/url").cloned().unwrap_or(Value::Null),
                    "title": snapshot.pointer("/snapshot/title").cloned().unwrap_or(Value::Null),
                }))
            }
            "page.screenshot" => {
                let tab_id = self.tab_id(&args).await?;
                self.call_native("browser.screenshot", json!({ "tab_id": tab_id }))
                    .await
            }
            "page.evaluate" => {
                let tab_id = self.tab_id(&args).await?;
                let source = required_string(&args, "source")?;
                let snapshot = self.snapshot(tab_id, false, false).await?;
                let frame = snapshot
                    .pointer("/snapshot/root/ref/frame")
                    .cloned()
                    .ok_or_else(|| "the active page has no evaluable main frame".to_string())?;
                let result = self
                    .call_native("browser.eval", json!({ "frame": frame, "js": source }))
                    .await?;
                Ok(parse_eval_result(result))
            }
            "page.wait" => {
                let ms = args
                    .get("ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(30_000);
                tokio::select! {
                    _ = self.cancel.cancelled() => Err("browser execution cancelled".to_string()),
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(json!({ "waited_ms": ms })),
                }
            }
            "page.waitForLoadState" => self.wait_for_load_state(args).await,
            "page.waitForURL" => self.wait_for_url(args).await,
            "page.waitForUrlChange" => self.wait_for_url_change(args).await,
            "page.waitForSelector" => self.wait_for_selector(args).await,
            "page.waitForFunction" => self.wait_for_function(args).await,
            "page.watchNetwork" => {
                let tab_id = self.tab_id(&args).await?;
                self.call_native("browser.network_cursor", json!({ "tab_id": tab_id }))
                    .await
            }
            "page.pollNetwork" => self.poll_network(args).await,
            "locator.waitFor" => self.wait_for_selector(args).await,
            "locator.setInputFiles" => {
                let tab_id = self.tab_id(&args).await?;
                let paths = args.get("paths").cloned().unwrap_or_else(|| json!([]));
                self.call_native(
                    "browser.set_input_files",
                    json!({ "tab_id": tab_id, "paths": paths }),
                )
                .await
            }
            name if name.starts_with("locator.") => self.locator_operation(name, args).await,
            name if name.starts_with("credentials.") => self.credential_operation(name, args).await,
            "dialog.handle" => {
                self.call_native(
                    "browser.handle_dialog",
                    json!({
                        "handle": required_string(&args, "handle")?,
                        "accept": args.get("accept").and_then(Value::as_bool).unwrap_or(true),
                        "prompt_text": args.get("prompt_text").cloned().unwrap_or(Value::Null),
                    }),
                )
                .await
            }
            "fileChooser.setFiles" => {
                self.call_native(
                    "browser.handle_file_chooser",
                    json!({
                        "handle": required_string(&args, "handle")?,
                        "paths": args.get("paths").cloned().unwrap_or_else(|| json!([])),
                    }),
                )
                .await
            }
            "keyboard.press" => {
                let tab_id = self.tab_id(&args).await?;
                let (key, modifiers) = parse_key_combo(&required_string(&args, "key")?);
                let baseline = self.snapshot(tab_id, false, false).await.ok();
                let action = self
                    .call_native(
                        "browser.key",
                        json!({ "tab_id": tab_id, "key": key, "modifiers": modifiers }),
                    )
                    .await?;
                self.observe_after_action(tab_id, baseline, action, false)
                    .await
            }
            "mouse.move" | "mouse.click" | "mouse.down" | "mouse.up" => {
                let tab_id = self.tab_id(&args).await?;
                let protocol = match name {
                    "mouse.move" => "browser.mouse_move",
                    "mouse.click" => "browser.mouse_click",
                    "mouse.down" => "browser.mouse_down",
                    _ => "browser.mouse_up",
                };
                let current_position = *self.mouse_position.lock().expect("mouse mutex poisoned");
                let x = args
                    .get("x")
                    .and_then(Value::as_f64)
                    .map(|value| value.round() as i64)
                    .unwrap_or(current_position.0);
                let y = args
                    .get("y")
                    .and_then(Value::as_f64)
                    .map(|value| value.round() as i64)
                    .unwrap_or(current_position.1);
                *self.mouse_position.lock().expect("mouse mutex poisoned") = (x, y);
                let options = args.get("options").cloned().unwrap_or_else(|| json!({}));
                let baseline = if name != "mouse.move" {
                    self.snapshot(tab_id, false, false).await.ok()
                } else {
                    None
                };
                let mouse_arguments = |point: Value| {
                    json!({
                        "tab_id": tab_id,
                        "point": point,
                        "button": mouse_button(&options),
                        "click_count": options.get("clickCount").and_then(Value::as_u64).unwrap_or(1),
                    })
                };
                let point = json!({ "x": x, "y": y });
                let action = match self
                    .call_native(protocol, mouse_arguments(point.clone()))
                    .await
                {
                    Ok(action) => action,
                    Err(error) => {
                        let adjusted = adjusted_point_for_viewport(&point, &error)
                            .ok_or_else(|| error.clone())?;
                        let adjusted_x = adjusted
                            .get("x")
                            .and_then(Value::as_f64)
                            .unwrap_or(x as f64)
                            .round() as i64;
                        let adjusted_y = adjusted
                            .get("y")
                            .and_then(Value::as_f64)
                            .unwrap_or(y as f64)
                            .round() as i64;
                        *self.mouse_position.lock().expect("mouse mutex poisoned") =
                            (adjusted_x, adjusted_y);
                        self.call_native(protocol, mouse_arguments(adjusted))
                            .await?
                    }
                };
                if name == "mouse.move" {
                    Ok(action)
                } else {
                    self.observe_after_action(tab_id, baseline, action, false)
                        .await
                }
            }
            "mouse.wheel" => {
                let tab_id = self.tab_id(&args).await?;
                // Playwright takes any delta. The native layer caps at
                // ±kMaxScrollDelta, and rejecting an over-large scroll outright
                // cost a round trip to learn a bound that is not in any
                // Playwright documentation — clamp to the top of the page
                // instead, which is what the caller meant.
                let clamp = |delta: f64| delta.round().clamp(-10_000.0, 10_000.0) as i64;
                let dx = clamp(args.get("dx").and_then(Value::as_f64).unwrap_or(0.0));
                let dy = clamp(args.get("dy").and_then(Value::as_f64).unwrap_or(0.0));
                let baseline = self.snapshot(tab_id, false, false).await.ok();
                let current_position = *self.mouse_position.lock().expect("mouse mutex poisoned");
                let action = self
                    .call_native(
                        "browser.scroll",
                        json!({
                            "tab_id": tab_id,
                            "point": { "x": current_position.0, "y": current_position.1 },
                            "dx": dx,
                            "dy": dy,
                        }),
                    )
                    .await?;
                self.observe_after_action(tab_id, baseline, action, false)
                    .await
            }
            "mouse.drag" => {
                let tab_id = self.tab_id(&args).await?;
                let baseline = self.snapshot(tab_id, false, false).await.ok();
                let action = self
                    .call_native(
                        "browser.mouse_drag",
                        json!({
                            "tab_id": tab_id,
                            "from": point_from_value(args.get("from"))?,
                            "to": point_from_value(args.get("to"))?,
                            "button": mouse_button(args.get("options").unwrap_or(&Value::Null)),
                            "steps": args.pointer("/options/steps").and_then(Value::as_u64).unwrap_or(8),
                        }),
                    )
                    .await?;
                self.observe_after_action(tab_id, baseline, action, false)
                    .await
            }
            _ => Err(format!("unsupported Steadwright operation: {name}")),
        }
    }

    async fn list_tabs(&self) -> Result<Value, String> {
        let content = self.call_native("browser.list_tabs", json!({})).await?;
        Ok(content.get("tabs").cloned().unwrap_or_else(|| json!([])))
    }

    fn current_tab_id(&self, args: &Value) -> Option<i32> {
        positive_i32(args.get("tab_id")).or_else(|| {
            self.default_tab_id
                .lock()
                .expect("tab mutex poisoned")
                .filter(|tab_id| *tab_id > 0)
        })
    }

    async fn open_agent_page(&self, url: &str) -> Result<Value, String> {
        let result = self
            .call_native(
                "browser.open_tab",
                json!({ "url": url, "agent_owned": true }),
            )
            .await?;
        let tab_id = value_tab_id(&result)
            .ok_or_else(|| "browser opened a tab without returning a valid tab id".to_string())?;
        *self.default_tab_id.lock().expect("tab mutex poisoned") = Some(tab_id);
        Ok(result)
    }

    async fn open_agent_page_and_observe(&self, url: &str) -> Result<Value, String> {
        let action = self.open_agent_page(url).await?;
        let tab_id = value_tab_id(&action)
            .ok_or_else(|| "browser opened a tab without returning a valid tab id".to_string())?;
        self.observe_navigation(tab_id, url, None, action).await
    }

    async fn observe_navigation(
        &self,
        tab_id: i32,
        expected_url: &str,
        baseline: Option<Value>,
        action: Value,
    ) -> Result<Value, String> {
        let baseline_fingerprint = baseline.as_ref().map(browser_snapshot_fingerprint);
        let deadline = tokio::time::Instant::now() + NAVIGATION_SETTLE_TIMEOUT;
        let mut after: Option<Value>;
        let mut previous_fingerprint: Option<u64> = None;
        let mut settle_deadline: Option<tokio::time::Instant> = None;
        loop {
            match self.snapshot(tab_id, false, false).await {
                Ok(snapshot) => {
                    let reached = snapshot
                        .pointer("/snapshot/url")
                        .and_then(Value::as_str)
                        .is_some_and(|actual| navigation_target_reached(actual, expected_url));
                    let changed = baseline_fingerprint
                        .map(|fingerprint| fingerprint != browser_snapshot_fingerprint(&snapshot))
                        .unwrap_or(true);
                    // Reaching the URL is not the same as having a page. A
                    // client-rendered store grid commits its navigation with a
                    // near-empty tree — 45 nodes against the 847 it settles at
                    // — and returning there hands the model a page whose
                    // controls genuinely are not in the snapshot yet. It reads
                    // that as "this page has no product links" and burns a
                    // round trip rediscovering them. Require the tree to stop
                    // growing before calling the navigation done.
                    let fingerprint = browser_snapshot_fingerprint(&snapshot);
                    let settled = previous_fingerprint
                        .as_ref()
                        .is_some_and(|previous| *previous == fingerprint);
                    previous_fingerprint = Some(fingerprint);
                    after = Some(snapshot);
                    if reached && changed {
                        // Bounded, because plenty of store pages never go
                        // quiet — a carousel or an analytics beacon keeps the
                        // tree moving forever, and waiting the full navigation
                        // budget for silence would be the same mistake as
                        // networkidle on a marketing page.
                        let grace_started = *settle_deadline.get_or_insert_with(|| {
                            tokio::time::Instant::now() + NAVIGATION_CONTENT_GRACE
                        });
                        if settled || tokio::time::Instant::now() >= grace_started {
                            break;
                        }
                    }
                }
                Err(error) if !is_transient_snapshot_error(&error) => return Err(error),
                Err(error) => {
                    // A committed Chromium navigation can briefly replace its frame before
                    // the accessibility tree is ready. Preserve the successful navigation
                    // and, critically, its tab id so the persistent Page follows a recovery
                    // tab instead of reverting to the stale source tab on the next exec.
                    return Ok(json!({
                        "tab_id": tab_id,
                        "action": action,
                        "after": Value::Null,
                        "observation": "navigation_pending",
                        "snapshot_error": error,
                    }));
                }
            }
            if self.cancel.is_cancelled() {
                return Err("browser execution cancelled".to_string());
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let Some(after) = after else {
            return Ok(json!({
                "tab_id": tab_id,
                "action": action,
                "after": Value::Null,
                "observation": "navigation_pending",
            }));
        };
        self.perception.record_snapshot(tab_id, &after);
        self.remember_observation(&after);
        let reached = after
            .pointer("/snapshot/url")
            .and_then(Value::as_str)
            .is_some_and(|actual| navigation_target_reached(actual, expected_url));
        Ok(json!({
            "tab_id": tab_id,
            "action": action,
            "after": after,
            "observation": if reached { "navigation_complete" } else { "navigation_still_loading" },
        }))
    }

    async fn tab_id(&self, args: &Value) -> Result<i32, String> {
        if let Some(tab_id) = positive_i32(args.get("tab_id")) {
            *self.default_tab_id.lock().expect("tab mutex poisoned") = Some(tab_id);
            return Ok(tab_id);
        }
        if let Some(tab_id) = self
            .default_tab_id
            .lock()
            .expect("tab mutex poisoned")
            .filter(|tab_id| *tab_id > 0)
        {
            return Ok(tab_id);
        }
        let tabs = self.list_tabs().await?;
        let tab_id = tabs
            .as_array()
            .and_then(|tabs| {
                tabs.iter()
                    .find(|tab| tab.get("active").and_then(Value::as_bool) == Some(true))
                    .or_else(|| tabs.first())
            })
            .and_then(value_tab_id)
            .ok_or_else(|| {
                "no browser tab is available; attach a tab or call context.newPage()".to_string()
            })?;
        *self.default_tab_id.lock().expect("tab mutex poisoned") = Some(tab_id);
        Ok(tab_id)
    }

    async fn snapshot(
        &self,
        tab_id: i32,
        include_bounds: bool,
        include_values: bool,
    ) -> Result<Value, String> {
        let arguments = json!({
            "tab_id": tab_id,
            "max_nodes": MAX_BROWSER_REPL_SNAPSHOT_NODES,
            "include_bounds": include_bounds,
            "include_values": include_values,
        });
        let mut last_error = None;
        for delay in [0, 120, 360, 800, 1_500, 2_500, 4_000] {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            match self
                .call_native("browser.snapshot", arguments.clone())
                .await
            {
                Ok(snapshot) => return Ok(snapshot),
                Err(error) if is_transient_snapshot_error(&error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
            if self.cancel.is_cancelled() {
                return Err("browser execution cancelled".to_string());
            }
        }
        Err(last_error.unwrap_or_else(|| "browser snapshot was unavailable".to_string()))
    }

    /// Flat, ref-tagged view of just the actionable elements.
    ///
    /// A full accessibility tree is mostly chrome the model will never act on.
    /// This returns only what can be clicked, typed into, or toggled, each with
    /// a stable `@eN` handle it can pass straight back to `page.locator`.
    fn interactive_snapshot(&self, tab_id: i32, snapshot: &Value) -> Value {
        let mut handles = HashMap::new();
        let mut seen: HashMap<(String, String), usize> = HashMap::new();
        let mut lines = Vec::new();
        let mut next_handle = 1usize;
        if let Some(root) = snapshot.pointer("/snapshot/root") {
            render_nodes(
                root,
                0,
                None,
                "",
                &mut next_handle,
                &mut handles,
                &mut seen,
                &mut lines,
            );
        }

        let url = snapshot
            .pointer("/snapshot/url")
            .cloned()
            .unwrap_or(Value::Null);
        let title = snapshot
            .pointer("/snapshot/title")
            .cloned()
            .unwrap_or(Value::Null);

        // The native side caps a snapshot at kDefaultMaxSnapshotNodes and sets
        // this flag when it clips. Left unsaid, a clipped page is
        // indistinguishable from a complete one: the model filters for a
        // control that really is on the page, finds nothing, and concludes the
        // page does not have it.
        let truncated = snapshot
            .pointer("/snapshot/truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let node_count = snapshot
            .pointer("/snapshot/node_count")
            .and_then(Value::as_u64);

        let mut text = String::new();
        if let Some(title) = title.as_str() {
            text.push_str(&format!("- title: {title:?}"));
            if let Some(url) = url.as_str() {
                text.push_str(&format!(" [url={url}]"));
            }
            text.push('\n');
        }
        if truncated {
            let counted = node_count
                .map(|count| format!(" at {count} nodes"))
                .unwrap_or_default();
            text.push_str(&format!(
                "- note: this tree was truncated{counted}; elements below the cut are missing. Scope the snapshot to a container, or locate by role and name instead of scanning this list.\n"
            ));
        }
        for line in &lines {
            text.push_str(&"  ".repeat(line.depth));
            text.push_str(&line.text);
            text.push('\n');
        }

        // The slim element list stays available so the model can filter in the
        // REPL without a round trip; unlike the old shape it omits every
        // attribute that does not apply to the node.
        let mut candidates = Vec::new();
        if let Some(root) = snapshot.pointer("/snapshot/root") {
            collect_candidates(root, None, &mut candidates);
        }
        let elements: Vec<Value> = candidates
            .iter()
            .filter(|candidate| is_interactive_role(&candidate.role) || candidate.checked.is_some())
            .enumerate()
            .map(|(index, candidate)| {
                let mut element = serde_json::Map::new();
                element.insert("ref".into(), json!(format!("e{}", index + 1)));
                // The raw AX role, not a normalized one. Filtering elements by
                // `e.role === 'radioButton'` is the ordinary thing to write,
                // and lowercasing it here broke exactly those filters while
                // leaving `'button'` working — a silent empty result rather
                // than an error.
                element.insert("role".into(), json!(candidate.role));
                element.insert("name".into(), json!(candidate.name));
                if let Some(checked) = candidate.checked {
                    element.insert("checked".into(), json!(checked));
                }
                if candidate.disabled {
                    element.insert("disabled".into(), json!(true));
                }
                if let Some(value) = &candidate.value {
                    element.insert("value".into(), json!(value));
                }
                if let Some(href) = &candidate.href {
                    element.insert("href".into(), json!(href));
                }
                if let Some(group) = &candidate.group {
                    element.insert("group".into(), json!(group));
                }
                Value::Object(element)
            })
            .collect();

        let diff = self
            .perception
            .take_previous_snapshot_text(tab_id)
            .map(|previous| unified_diff(&previous, &text))
            .unwrap_or_default();
        self.perception.store_snapshot_text(tab_id, text.clone());
        self.perception.store_ref_handles(tab_id, handles);

        json!({
            "tab_id": tab_id,
            "url": url,
            "title": title,
            "interactive": true,
            "truncated": truncated,
            "text": text,
            "diff": diff,
            "elements": elements,
        })
    }

    /// Turn an `@eN` handle back into a locator query.
    ///
    /// Resolution runs against the current snapshot by role and name, so a
    /// handle still points at the right control after the click that minted it
    /// re-rendered the page.
    fn resolve_ref_query(&self, tab_id: i32, query: &Value) -> Result<(Value, usize), String> {
        let handle = query
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| "ref locator is missing its handle".to_string())?;
        let (role, name, occurrence) = self
            .perception
            .lookup_ref_handle(tab_id, handle)
            .ok_or_else(|| {
                format!(
                    "unknown element handle {handle}; call page.snapshot({{interactive: true}}) \
                     to mint fresh handles for this tab"
                )
            })?;
        Ok((
            json!({ "kind": "role", "role": role, "name": name, "exact": true }),
            occurrence,
        ))
    }

    async fn current_url(&self, tab_id: i32) -> Result<String, String> {
        let snapshot = self.snapshot(tab_id, false, false).await?;
        snapshot
            .pointer("/snapshot/url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "the active page has no observable URL".to_string())
    }

    async fn page_frame(&self, tab_id: i32) -> Result<Value, String> {
        let snapshot = self.snapshot(tab_id, false, false).await?;
        snapshot
            .pointer("/snapshot/root/ref/frame")
            .cloned()
            .ok_or_else(|| "the active page has no evaluable main frame".to_string())
    }

    fn cancelled(&self) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err("browser execution cancelled".to_string());
        }
        Ok(())
    }

    /// `page.waitForURL(url)` for string and RegExp patterns, matched natively.
    /// Function predicates run in QuickJS and drive `page.waitForUrlChange`
    /// instead, so the predicate still evaluates where the model wrote it.
    async fn wait_for_url(&self, args: Value) -> Result<Value, String> {
        let tab_id = self.tab_id(&args).await?;
        let pattern = args
            .get("pattern")
            .cloned()
            .ok_or_else(|| "page.waitForURL requires a url pattern".to_string())?;
        let timeout = wait_timeout(&args, DEFAULT_WAIT_TIMEOUT);
        let wait = WaitLoop::new(timeout, WAIT_POLL_INTERVAL);
        loop {
            let url = self.current_url(tab_id).await?;
            if url_pattern_matches(&pattern, &url) {
                return Ok(json!({ "url": url }));
            }
            self.cancelled()?;
            if !wait.tick().await {
                return Err(format!(
                    "page.waitForURL timed out after {}ms waiting for {}; the page is still at {url}",
                    timeout.as_millis(),
                    describe_url_pattern(&pattern),
                ));
            }
        }
    }

    /// Block until the tab's URL differs from `since`. One operation per actual
    /// navigation, which keeps a JS-side predicate loop far under the
    /// per-execution operation cap.
    async fn wait_for_url_change(&self, args: Value) -> Result<Value, String> {
        let tab_id = self.tab_id(&args).await?;
        let since = args
            .get("since")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let timeout = wait_timeout(&args, DEFAULT_WAIT_TIMEOUT);
        let wait = WaitLoop::new(timeout, WAIT_POLL_INTERVAL);
        loop {
            let url = self.current_url(tab_id).await?;
            if url != since {
                return Ok(json!({ "url": url }));
            }
            self.cancelled()?;
            if !wait.tick().await {
                break;
            }
        }
        Err(format!(
            "timed out after {}ms waiting for the page to navigate away from {since}",
            timeout.as_millis(),
        ))
    }

    /// Backs both `page.waitForSelector` and `locator.waitFor`.
    async fn wait_for_selector(&self, args: Value) -> Result<Value, String> {
        let tab_id = self.tab_id(&args).await?;
        let query = args
            .get("query")
            .cloned()
            .ok_or_else(|| "locator query is missing".to_string())?;
        let state = args
            .pointer("/options/state")
            .and_then(Value::as_str)
            .unwrap_or("visible")
            .to_string();
        let expect_present = wait_state_expects_presence(&state)?;
        let timeout = wait_timeout(&args, DEFAULT_WAIT_TIMEOUT);
        let wait = WaitLoop::new(timeout, WAIT_POLL_INTERVAL);
        loop {
            let snapshot = self.snapshot(tab_id, false, false).await?;
            let matches = matching_nodes(&snapshot, &query).len();
            if (matches > 0) == expect_present {
                return Ok(json!({ "state": state, "matches": matches }));
            }
            self.cancelled()?;
            if !wait.tick().await {
                return Err(format!(
                    "timed out after {}ms waiting for state {state:?}; {matches} elements currently match",
                    timeout.as_millis(),
                ));
            }
        }
    }

    /// `page.waitForFunction`. The predicate runs in the page, exactly as it
    /// does in Playwright, so `document` and `window` are the page's own.
    async fn wait_for_function(&self, args: Value) -> Result<Value, String> {
        let tab_id = self.tab_id(&args).await?;
        let source = required_string(&args, "source")?;
        let timeout = wait_timeout(&args, DEFAULT_WAIT_TIMEOUT);
        let interval = wait_poll_interval(&args);
        let wait = WaitLoop::new(timeout, interval);
        let mut frame = self.page_frame(tab_id).await?;
        loop {
            let evaluated = match self
                .call_native("browser.eval", json!({ "frame": frame, "js": source }))
                .await
            {
                Ok(result) => Ok(parse_eval_result(result)),
                // A navigation between polls retires the frame handle. Re-resolve
                // once and let the next poll run against the new document rather
                // than failing a wait that was doing its job.
                Err(error) if is_stale_snapshot_error(&error) || is_missing_tab_error(&error) => {
                    frame = self.page_frame(tab_id).await?;
                    Err(error)
                }
                Err(error) => return Err(error),
            };
            if let Ok(value) = evaluated {
                if is_truthy(&value) {
                    return Ok(value);
                }
            }
            self.cancelled()?;
            if !wait.tick().await {
                break;
            }
        }
        Err(format!(
            "page.waitForFunction timed out after {}ms; the predicate never returned a truthy value",
            timeout.as_millis(),
        ))
    }

    /// Block until the tab's network log grows past `after_event_id`.
    ///
    /// With a `pattern`, matching happens natively and the whole wait is one
    /// operation. Without one — the model passed a predicate function — this
    /// returns each new batch so QuickJS can apply the predicate where it was
    /// written, costing one operation per burst of network activity rather
    /// than one per poll.
    async fn poll_network(&self, args: Value) -> Result<Value, String> {
        let tab_id = self.tab_id(&args).await?;
        let mut cursor = args
            .get("after_event_id")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let kind = args
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("response")
            .to_string();
        let pattern = args
            .get("pattern")
            .cloned()
            .filter(|pattern| !pattern.is_null());
        let timeout = wait_timeout(&args, DEFAULT_WAIT_TIMEOUT);
        let wait = WaitLoop::new(timeout, WAIT_POLL_INTERVAL);
        loop {
            let page = self
                .call_native(
                    "browser.network_log",
                    json!({ "tab_id": tab_id, "after_event_id": cursor }),
                )
                .await?;
            let records: Vec<Value> = page
                .get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(next) = page.get("cursor").and_then(Value::as_i64) {
                cursor = cursor.max(next);
            }
            let relevant: Vec<Value> = records
                .into_iter()
                .filter(|record| network_record_is_kind(record, &kind))
                .collect();
            match &pattern {
                Some(pattern) => {
                    if let Some(hit) = relevant.iter().find(|record| {
                        record
                            .get("url")
                            .and_then(Value::as_str)
                            .is_some_and(|url| url_pattern_matches(pattern, url))
                    }) {
                        return Ok(json!({ "record": hit, "cursor": cursor }));
                    }
                }
                None => {
                    if !relevant.is_empty() {
                        return Ok(json!({ "records": relevant, "cursor": cursor }));
                    }
                }
            }
            self.cancelled()?;
            if !wait.tick().await {
                return Err(match &pattern {
                    Some(pattern) => format!(
                        "page.waitFor{} timed out after {}ms waiting for {}",
                        if kind == "request" {
                            "Request"
                        } else {
                            "Response"
                        },
                        timeout.as_millis(),
                        describe_url_pattern(pattern),
                    ),
                    None => format!(
                        "page.waitFor{} timed out after {}ms; no matching network activity was observed",
                        if kind == "request" {
                            "Request"
                        } else {
                            "Response"
                        },
                        timeout.as_millis(),
                    ),
                });
            }
        }
    }

    async fn wait_for_load_state(&self, args: Value) -> Result<Value, String> {
        let tab_id = self.tab_id(&args).await?;
        let state = args
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("load")
            .to_string();
        let timeout = wait_timeout(&args, DEFAULT_WAIT_TIMEOUT);
        let predicate = match state.as_str() {
            // Until the Chromium network observer lands, "no further accessibility
            // change" is the honest stand-in for network idle.
            "networkidle" => return self.wait_for_settled_snapshot(tab_id, timeout).await,
            "domcontentloaded" => "document.readyState !== 'loading'",
            "load" => "document.readyState === 'complete'",
            other => {
                return Err(format!(
                    "unsupported load state {other:?}; use load, domcontentloaded, or networkidle"
                ));
            }
        };
        self.wait_for_function(json!({
            "tab_id": tab_id,
            "source": predicate,
            "options": { "timeout": timeout.as_millis() as u64 },
        }))
        .await
        .map(|_| json!({ "state": state }))
        // Keep the underlying cause. Reporting every failure as a timeout hides
        // the difference between "the page really never reached this state" and
        // "the readyState probe could not run at all", which are opposite fixes.
        .map_err(|error| format!("page.waitForLoadState({state:?}) failed: {error}"))
    }

    /// Poll snapshots until two consecutive observations are identical.
    async fn wait_for_settled_snapshot(
        &self,
        tab_id: i32,
        timeout: Duration,
    ) -> Result<Value, String> {
        let wait = WaitLoop::new(timeout, WAIT_POLL_INTERVAL);
        let mut result = self.snapshot(tab_id, false, false).await?;
        let mut fingerprint = browser_snapshot_fingerprint(&result);
        while wait.tick().await {
            self.cancelled()?;
            let next = self.snapshot(tab_id, false, false).await?;
            let next_fingerprint = browser_snapshot_fingerprint(&next);
            result = next;
            if next_fingerprint == fingerprint {
                break;
            }
            fingerprint = next_fingerprint;
        }
        self.remember_observation(&result);
        Ok(result)
    }

    async fn locator_operation(&self, name: &str, args: Value) -> Result<Value, String> {
        let tab_id = self.tab_id(&args).await?;
        let mut resolved_query = None;
        let mut ref_occurrence = None;
        if args.pointer("/query/kind").and_then(Value::as_str) == Some("ref") {
            let query = args
                .get("query")
                .ok_or_else(|| "locator query is missing".to_string())?;
            let (effective, occurrence) = self.resolve_ref_query(tab_id, query)?;
            resolved_query = Some(effective);
            ref_occurrence = Some(occurrence);
        }
        let query = resolved_query
            .as_ref()
            .or_else(|| args.get("query"))
            .ok_or_else(|| "locator query is missing".to_string())?;
        // A bare locator sends no index. `.first()`, `.nth(i)`, and `.last()`
        // send one, which is how the model opts out of strict mode — the same
        // contract Playwright has.
        let explicit_index = args
            .get("index")
            .and_then(Value::as_u64)
            .map(|index| index as usize);
        let index = ref_occurrence.or(explicit_index).unwrap_or(0);
        let use_last = args.get("last").and_then(Value::as_bool).unwrap_or(false);
        let strict = ref_occurrence.is_none() && explicit_index.is_none() && !use_last;
        let include_bounds = matches!(
            name,
            "locator.bounds"
                | "locator.hover"
                | "locator.dblclick"
                | "locator.click"
                | "locator.check"
                | "locator.uncheck"
        );
        let include_values = matches!(
            name,
            "locator.fill" | "locator.type" | "locator.clear" | "locator.value"
        );
        let mut snapshot = self
            .snapshot(tab_id, include_bounds, include_values)
            .await?;
        let mut candidates = matching_nodes(&snapshot, query);
        match name {
            "locator.count" => return Ok(json!(candidates.len())),
            "locator.texts" => {
                return Ok(Value::Array(
                    candidates
                        .iter()
                        .map(|candidate| json!(candidate.name))
                        .collect(),
                ));
            }
            "locator.elements" => {
                let mut action_candidates = candidates.clone();
                prioritize_action_candidates(&mut action_candidates, query, "locator.check");
                return Ok(Value::Array(
                    candidates
                        .iter()
                        .enumerate()
                        .map(|(index, candidate)| {
                            let action_index = action_candidates
                                .iter()
                                .position(|action_candidate| {
                                    action_candidate.reference == candidate.reference
                                })
                                .unwrap_or(index);
                            json!({
                                "id": candidate
                                    .reference
                                    .get("ax_node_id")
                                    .map(Value::to_string)
                                    .unwrap_or_else(|| format!("stead-{index}")),
                                "role": candidate.role,
                                "name": candidate.name,
                                "href": candidate.href,
                                "value": candidate.value,
                                "checked": candidate.checked,
                                "disabled": candidate.disabled,
                                "group": candidate.group,
                                "action_index": action_index,
                            })
                        })
                        .collect(),
                ));
            }
            _ => {}
        }
        if candidates.is_empty() {
            candidates = compatible_action_candidates(&snapshot, query, name);
        }
        prioritize_action_candidates(&mut candidates, query, name);
        // Two failures look alike in a snapshot and must not be treated alike.
        //
        // Matching nothing, or matching only disabled nodes, is *transient*: a
        // live page mounts controls late and configurators enable the next
        // choice after the previous selection commits. Polling is the correct
        // response, and it is what keeps the model out of the loop.
        //
        // Matching several nodes is *permanent*. No amount of waiting resolves
        // it, so it fails immediately below with the candidates named.
        let actionability_timeout = wait_timeout(&args, ACTIONABILITY_TIMEOUT);
        if is_locator_action_operation(name)
            && (candidates.is_empty() || candidates.iter().all(|candidate| candidate.disabled))
        {
            let wait = WaitLoop::new(actionability_timeout, WAIT_POLL_INTERVAL);
            while wait.tick().await {
                self.cancelled()?;
                let refreshed = self
                    .snapshot(tab_id, include_bounds, include_values)
                    .await?;
                let mut refreshed_candidates = matching_nodes(&refreshed, query);
                if refreshed_candidates.is_empty() {
                    refreshed_candidates = compatible_action_candidates(&refreshed, query, name);
                }
                prioritize_action_candidates(&mut refreshed_candidates, query, name);
                snapshot = refreshed;
                candidates = refreshed_candidates;
                if candidates.iter().any(|candidate| !candidate.disabled) {
                    break;
                }
            }
        }
        if strict && candidates.len() > 1 {
            return Err(format!(
                "locator resolved to {} elements and Stead will not guess between them: {}. \
                 Narrow it with .filter({{hasText}}), a more specific role or accessible name, \
                 or select one explicitly with .first(), .nth(i), or .last().",
                candidates.len(),
                candidate_summary(&candidates),
            ));
        }
        let candidate = if use_last {
            candidates.last()
        } else {
            candidates.get(index)
        }
        .ok_or_else(|| {
            if candidates.is_empty() {
                format!(
                    "locator matched no elements after waiting {}ms. Confirm the page finished \
                     loading (page.waitForLoadState), that the right tab is active, and that no \
                     modal or overlay is covering it, then correct the locator — repeating a \
                     near-identical one will fail the same way.",
                    actionability_timeout.as_millis(),
                )
            } else {
                format!(
                    "locator matched {} elements, so {} is out of range: {}",
                    candidates.len(),
                    if use_last {
                        "the last match".to_string()
                    } else {
                        format!("index {index}")
                    },
                    candidate_summary(&candidates),
                )
            }
        })?
        .clone();
        match name {
            "locator.text" => Ok(json!(candidate.name)),
            "locator.enabled" => Ok(json!(!candidate.disabled)),
            "locator.checked" => Ok(candidate.checked.map(Value::Bool).unwrap_or(Value::Null)),
            "locator.value" => Ok(candidate
                .value
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null)),
            "locator.attribute" => {
                let name = required_string(&args, "name")?;
                match name.to_ascii_lowercase().as_str() {
                    "href" => {
                        return Ok(candidate
                            .href
                            .clone()
                            .map(Value::String)
                            .unwrap_or(Value::Null));
                    }
                    "aria-checked" => {
                        return Ok(candidate
                            .checked
                            .map(|checked| Value::String(checked.to_string()))
                            .unwrap_or(Value::Null));
                    }
                    "aria-disabled" | "disabled" => {
                        return Ok(if candidate.disabled {
                            Value::String("true".to_string())
                        } else {
                            Value::Null
                        });
                    }
                    _ => {}
                }
                let probe = self
                    .call_native("browser.probe_node", json!({ "ref": candidate.reference }))
                    .await?;
                let key = match name.to_ascii_lowercase().as_str() {
                    "id" => Some("id"),
                    "class" => Some("class_name"),
                    "name" => Some("name_attr"),
                    "type" => Some("type_attr"),
                    "role" => Some("role_attr"),
                    "aria-label" => Some("aria_label"),
                    _ => None,
                };
                let probed = key
                    .and_then(|key| probe.pointer(&format!("/probe/{key}")))
                    .cloned()
                    .unwrap_or(Value::Null);
                if name.eq_ignore_ascii_case("aria-label")
                    && probed.as_str().is_none_or(str::is_empty)
                    && !candidate.name.is_empty()
                {
                    Ok(Value::String(candidate.name.clone()))
                } else {
                    Ok(probed)
                }
            }
            "locator.bounds" => Ok(candidate.bounds.clone().unwrap_or(Value::Null)),
            "locator.waitFor" => Ok(json!({ "visible": true, "matches": candidates.len() })),
            "locator.screenshot" => {
                self.call_native(
                    "browser.screenshot",
                    json!({ "tab_id": tab_id, "ref": candidate.reference }),
                )
                .await
            }
            "locator.hover" | "locator.dblclick" => {
                let bounds = candidate.bounds.as_ref().ok_or_else(|| {
                    "locator has no usable bounds; take a screenshot and use page.mouse coordinates"
                        .to_string()
                })?;
                let point = bounds_center(bounds)?;
                let protocol = if name == "locator.hover" {
                    "browser.mouse_move"
                } else {
                    "browser.mouse_click"
                };
                let action = self
                    .call_native(
                        protocol,
                        json!({
                            "tab_id": tab_id,
                            "point": point,
                            "button": 0,
                            "click_count": if name == "locator.dblclick" { 2 } else { 1 },
                        }),
                    )
                    .await?;
                if name == "locator.hover" {
                    Ok(action)
                } else {
                    self.observe_after_action(tab_id, Some(snapshot), action, false)
                        .await
                }
            }
            "locator.check" | "locator.uncheck" | "locator.click"
                if candidate.checked.is_some() =>
            {
                if candidate.disabled {
                    return Err(format!(
                        "locator resolved to disabled {} {:?}",
                        candidate.role, candidate.name
                    ));
                }
                let requested = match name {
                    "locator.check" => true,
                    "locator.uncheck" => false,
                    _ => !candidate.checked.unwrap_or(false),
                };
                if candidate.checked == Some(requested) {
                    return Ok(json!({ "changed": false, "checked": requested }));
                }
                let mut click_baseline = snapshot.clone();
                let action = match self
                    .call_native("browser.click", json!({ "ref": candidate.reference }))
                    .await
                {
                    Ok(action) => action,
                    Err(error) if is_stale_snapshot_error(&error) => {
                        click_baseline = self
                            .snapshot(tab_id, include_bounds, include_values)
                            .await?;
                        let mut refreshed = matching_nodes(&click_baseline, query);
                        if refreshed.is_empty() {
                            refreshed = compatible_action_candidates(&click_baseline, query, name);
                        }
                        prioritize_action_candidates(&mut refreshed, query, name);
                        let completed_candidate = if use_last {
                            refreshed.last()
                        } else {
                            refreshed.get(index)
                        };
                        if completed_candidate.and_then(|candidate| candidate.checked)
                            == Some(requested)
                        {
                            self.remember_observation(&click_baseline);
                            return Ok(json!({
                                "action": {"recovered": "stale_ref_after_completed_check"},
                                "after": click_baseline,
                                "observation": "progress",
                            }));
                        }
                        let refreshed_candidate = if use_last {
                            refreshed.last()
                        } else {
                            refreshed.get(index)
                        }
                        .ok_or_else(|| format!(
                            "locator disappeared while recovering from a stale snapshot: {} {:?}",
                            candidate.role, candidate.name
                        ))?;
                        self.call_native(
                            "browser.click",
                            json!({ "ref": refreshed_candidate.reference }),
                        )
                        .await?
                    }
                    Err(error) => return Err(error),
                };
                // Chromium's audited DOM-label activation runs in an isolated
                // world after the broker accepts the AX action. Give that
                // dispatch one animation-frame-sized completion window before
                // checking state; otherwise verification races the click and
                // needlessly falls through to slow coordinate recovery.
                tokio::time::sleep(Duration::from_millis(180)).await;
                let observed = self
                    .observe_after_action(tab_id, Some(click_baseline), action, false)
                    .await?;
                let after = observed.get("after").cloned().unwrap_or(Value::Null);
                let mut after_candidates = matching_nodes(&after, query);
                if after_candidates.is_empty() {
                    after_candidates = compatible_action_candidates(&after, query, name);
                }
                prioritize_action_candidates(&mut after_candidates, query, name);
                let after_candidate = if use_last {
                    after_candidates.last()
                } else {
                    after_candidates.get(index)
                };
                if after_candidate.and_then(|candidate| candidate.checked) == Some(requested) {
                    return Ok(observed);
                }

                // Playwright scrolls actionable controls into view before its
                // coordinate fallback. Re-resolve after scrolling because
                // dynamic configurators replace/reorder their radio nodes as
                // each dimension becomes enabled.
                let mut activation_candidate = candidate.clone();
                let mut activation_baseline = after.clone();
                if let Ok(scroll_action) = self
                    .call_native(
                        "browser.scroll_into_view",
                        json!({ "ref": candidate.reference }),
                    )
                    .await
                    && let Ok(scroll_observed) = self
                        .observe_after_action(tab_id, Some(after.clone()), scroll_action, false)
                        .await
                {
                    activation_baseline = scroll_observed
                        .get("after")
                        .cloned()
                        .unwrap_or_else(|| after.clone());
                    let mut scrolled_candidates = matching_nodes(&activation_baseline, query);
                    if scrolled_candidates.is_empty() {
                        scrolled_candidates =
                            compatible_action_candidates(&activation_baseline, query, name);
                    }
                    prioritize_action_candidates(&mut scrolled_candidates, query, name);
                    if let Some(scrolled_candidate) = if use_last {
                        scrolled_candidates.last()
                    } else {
                        scrolled_candidates.get(index)
                    } {
                        activation_candidate = scrolled_candidate.clone();
                    }
                }

                // AX tree bounds are document-relative on long pages, while
                // raw input consumes visible-viewport coordinates. Probe the
                // live DOM target first so custom shopping cards are clicked
                // at their current client rect instead of at a stale document
                // offset. Keep the AX bounds only as a last-resort fallback.
                let mut probe = self
                    .call_native(
                        "browser.probe_node",
                        json!({ "ref": activation_candidate.reference }),
                    )
                    .await
                    .ok();
                for _ in 0..3 {
                    if !probe.as_ref().is_some_and(probe_requires_wheel_scroll) {
                        break;
                    }
                    let y = probe
                        .as_ref()
                        .and_then(|probe| probe.pointer("/probe/client_rect/y"))
                        .and_then(Value::as_f64)
                        .unwrap_or(1.0);
                    let dy = if y < 0.0 { -640 } else { 640 };
                    let Ok(wheel_action) = self
                        .call_native(
                            "browser.scroll",
                            json!({ "tab_id": tab_id, "dx": 0, "dy": dy }),
                        )
                        .await
                    else {
                        break;
                    };
                    let Ok(wheel_observed) = self
                        .observe_after_action(
                            tab_id,
                            Some(activation_baseline.clone()),
                            wheel_action,
                            false,
                        )
                        .await
                    else {
                        break;
                    };
                    activation_baseline = wheel_observed
                        .get("after")
                        .cloned()
                        .unwrap_or_else(|| activation_baseline.clone());
                    let mut wheel_candidates = matching_nodes(&activation_baseline, query);
                    if wheel_candidates.is_empty() {
                        wheel_candidates =
                            compatible_action_candidates(&activation_baseline, query, name);
                    }
                    prioritize_action_candidates(&mut wheel_candidates, query, name);
                    if let Some(wheel_candidate) = if use_last {
                        wheel_candidates.last()
                    } else {
                        wheel_candidates.get(index)
                    } {
                        activation_candidate = wheel_candidate.clone();
                    }
                    probe = self
                        .call_native(
                            "browser.probe_node",
                            json!({ "ref": activation_candidate.reference }),
                        )
                        .await
                        .ok();
                }
                let point = probe
                    .as_ref()
                    .and_then(|probe| probe.pointer("/probe/client_rect"))
                    .filter(|bounds| usable_bounds(bounds))
                    .map(bounds_center)
                    .transpose()?
                    .or_else(|| {
                        activation_candidate
                            .bounds
                            .as_ref()
                            .filter(|bounds| usable_bounds(bounds))
                            .and_then(|bounds| bounds_center(bounds).ok())
                    })
                    .ok_or_else(|| {
                        format!(
                            "{} {:?} did not change state after semantic activation and has no coordinate fallback",
                            candidate.role, candidate.name
                        )
                    })?;
                let mouse_arguments = |point: Value| {
                    json!({
                        "tab_id": tab_id,
                        "point": point,
                        "button": 0,
                        "click_count": 1,
                    })
                };
                let mouse_action = match self
                    .call_native("browser.mouse_click", mouse_arguments(point.clone()))
                    .await
                {
                    Ok(action) => action,
                    Err(error) => {
                        let adjusted = adjusted_point_for_viewport(&point, &error)
                            .ok_or_else(|| error.clone())?;
                        self.call_native("browser.mouse_click", mouse_arguments(adjusted))
                            .await?
                    }
                };
                let mouse_observed = self
                    .observe_after_action(tab_id, Some(activation_baseline), mouse_action, false)
                    .await?;
                let mouse_after = mouse_observed.get("after").cloned().unwrap_or(Value::Null);
                let mut mouse_candidates = matching_nodes(&mouse_after, query);
                if mouse_candidates.is_empty() {
                    mouse_candidates = compatible_action_candidates(&mouse_after, query, name);
                }
                prioritize_action_candidates(&mut mouse_candidates, query, name);
                let mouse_candidate = if use_last {
                    mouse_candidates.last()
                } else {
                    mouse_candidates.get(index)
                };
                if mouse_candidate.and_then(|candidate| candidate.checked) == Some(requested) {
                    Ok(mouse_observed)
                } else {
                    Err(format!(
                        "{} {:?} did not change to checked={requested} after semantic and coordinate activation; probe={probe:?}; point={point}; ax_bounds={:?}",
                        candidate.role, candidate.name, candidate.bounds
                    ))
                }
            }
            // check/uncheck reach here when the target exposes no checked state
            // in the accessibility tree — common for custom radio and checkbox
            // widgets built from divs. Activating them is still the right
            // behavior, and it is what a user does; erroring as "unsupported"
            // sent the model hunting for workarounds one round trip at a time.
            "locator.click"
            | "locator.check"
            | "locator.uncheck"
            | "locator.fill"
            | "locator.type"
            | "locator.clear"
            | "locator.focus"
            | "locator.scrollIntoView" => {
                if candidate.disabled && name != "locator.scrollIntoView" {
                    return Err(format!(
                        "locator resolved to disabled {} {:?}",
                        candidate.role, candidate.name
                    ));
                }
                let link_target = (name == "locator.click")
                    .then(|| resolved_link_target(&snapshot, &candidate))
                    .flatten();
                // Standard semantic links already carry their canonical
                // destination. AX node geometry/identity can shift under
                // animated product grids and has caused a named link click to
                // activate a neighboring card. Navigate the preserved href
                // directly: it is deterministic, avoids a coordinate round
                // trip, and keeps click emulation for controls whose behavior
                // is not representable by a URL.
                if name == "locator.click"
                    && role_matches(&candidate.role, "link")
                    && let Some(target) = link_target.as_deref()
                {
                    let action = self
                        .call_native(
                            "browser.navigate",
                            json!({ "tab_id": tab_id, "url": target }),
                        )
                        .await?;
                    return self
                        .observe_navigation(tab_id, target, Some(snapshot), action)
                        .await;
                }
                let (protocol, arguments) = match name {
                    // check/uncheck land here only for controls with no
                    // accessible checked state; activating them is a click.
                    "locator.click" | "locator.check" | "locator.uncheck" => {
                        ("browser.click", json!({ "ref": candidate.reference }))
                    }
                    "locator.fill" | "locator.type" => (
                        "browser.fill",
                        json!({
                            "ref": candidate.reference,
                            "value": required_string(&args, "value")?,
                        }),
                    ),
                    "locator.clear" => (
                        "browser.fill",
                        json!({
                            "ref": candidate.reference,
                            "value": "",
                        }),
                    ),
                    "locator.focus" => ("browser.focus", json!({ "ref": candidate.reference })),
                    _ => (
                        "browser.scroll_into_view",
                        json!({ "ref": candidate.reference }),
                    ),
                };
                let action = match self.call_native(protocol, arguments.clone()).await {
                    Ok(action) => action,
                    Err(error) if is_stale_snapshot_error(&error) => {
                        let before_url = snapshot
                            .pointer("/snapshot/url")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        snapshot = self
                            .snapshot(tab_id, include_bounds, include_values)
                            .await?;
                        let after_url = snapshot
                            .pointer("/snapshot/url")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        if name == "locator.click"
                            && role_matches(&candidate.role, "link")
                            && before_url.is_some()
                            && after_url.is_some()
                            && before_url != after_url
                        {
                            self.remember_observation(&snapshot);
                            return Ok(json!({
                                "action": {"recovered": "stale_ref_after_navigation"},
                                "after": snapshot,
                                "observation": "progress",
                            }));
                        }
                        let mut refreshed = matching_nodes(&snapshot, query);
                        if refreshed.is_empty() {
                            refreshed = compatible_action_candidates(&snapshot, query, name);
                        }
                        prioritize_action_candidates(&mut refreshed, query, name);
                        let refreshed_candidate = if use_last {
                            refreshed.last()
                        } else {
                            refreshed.get(index)
                        }
                        .ok_or_else(|| format!(
                            "locator disappeared while recovering from a stale snapshot: {} {:?}",
                            candidate.role, candidate.name
                        ))?;
                        let refreshed_arguments = match name {
                            "locator.click" | "locator.focus" | "locator.scrollIntoView" => {
                                json!({ "ref": refreshed_candidate.reference })
                            }
                            _ => json!({
                                "ref": refreshed_candidate.reference,
                                "value": required_string(&args, "value")?,
                            }),
                        };
                        self.call_native(protocol, refreshed_arguments).await?
                    }
                    Err(error) => return Err(error),
                };
                let observed = self
                    .observe_after_action(tab_id, Some(snapshot.clone()), action, include_values)
                    .await?;
                if name == "locator.click"
                    && role_matches(&candidate.role, "link")
                    && observed.get("observation").and_then(Value::as_str) == Some("no_ax_progress")
                    && let Some(target) = link_target.as_deref()
                {
                    let navigation = self
                        .call_native(
                            "browser.navigate",
                            json!({ "tab_id": tab_id, "url": target }),
                        )
                        .await?;
                    return self
                        .observe_navigation(tab_id, target, Some(snapshot), navigation)
                        .await;
                }
                Ok(observed)
            }
            "locator.press" => {
                self.call_native("browser.focus", json!({ "ref": candidate.reference }))
                    .await?;
                let (key, modifiers) = parse_key_combo(&required_string(&args, "key")?);
                let action = self
                    .call_native(
                        "browser.key",
                        json!({ "tab_id": tab_id, "key": key, "modifiers": modifiers }),
                    )
                    .await?;
                self.observe_after_action(tab_id, Some(snapshot), action, false)
                    .await
            }
            _ => Err(format!("unsupported locator operation: {name}")),
        }
    }

    async fn credential_operation(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "credentials.list" => {
                let tab_id = self.tab_id(&args).await?;
                let origin = if let Some(origin) = args.get("origin").and_then(Value::as_str) {
                    origin.to_string()
                } else {
                    let snapshot = self.snapshot(tab_id, false, false).await?;
                    let url = snapshot
                        .pointer("/snapshot/url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "current page URL is unavailable".to_string())?;
                    url_origin(url)?
                };
                self.call_native(
                    "browser.list_credentials",
                    json!({ "tab_id": tab_id, "origin": origin }),
                )
                .await
            }
            "credentials.fill" => {
                let credential = args
                    .get("credential")
                    .cloned()
                    .ok_or_else(|| "credential handle is missing".to_string())?;
                let username = args
                    .get("username")
                    .ok_or_else(|| "username locator is missing".to_string())?;
                let password = args
                    .get("password")
                    .ok_or_else(|| "password locator is missing".to_string())?;
                let tab_id = descriptor_tab_id(username, &self.default_tab_id)?;
                let snapshot = self.snapshot(tab_id, false, false).await?;
                let username_ref = resolve_descriptor(&snapshot, username)?;
                let password_ref = resolve_descriptor(&snapshot, password)?;
                self.call_native(
                    "browser.fill_credential",
                    json!({
                        "credential": credential,
                        "username_field": username_ref,
                        "password_field": password_ref,
                    }),
                )
                .await
            }
            "credentials.fillTotp" => {
                let credential = args
                    .get("credential")
                    .cloned()
                    .ok_or_else(|| "credential handle is missing".to_string())?;
                let field = args
                    .get("field")
                    .ok_or_else(|| "TOTP field locator is missing".to_string())?;
                let tab_id = descriptor_tab_id(field, &self.default_tab_id)?;
                let snapshot = self.snapshot(tab_id, false, false).await?;
                let field_ref = resolve_descriptor(&snapshot, field)?;
                self.call_native(
                    "browser.fill_totp",
                    json!({ "credential": credential, "field": field_ref }),
                )
                .await
            }
            "credentials.markInjected" => {
                let tab_id = self.tab_id(&args).await?;
                let snapshot = self.snapshot(tab_id, false, false).await?;
                let frame = snapshot
                    .pointer("/snapshot/root/ref/frame")
                    .cloned()
                    .ok_or_else(|| "the active page has no main frame".to_string())?;
                self.call_native(
                    "browser.mark_credential_injection",
                    json!({ "frame": frame }),
                )
                .await
            }
            _ => Err(format!("unsupported credential operation: {name}")),
        }
    }

    async fn observe_after_action(
        &self,
        tab_id: i32,
        baseline: Option<Value>,
        action: Value,
        include_values: bool,
    ) -> Result<Value, String> {
        let baseline_fingerprint = baseline.as_ref().map(browser_snapshot_fingerprint);
        let mut after = self.snapshot(tab_id, false, include_values).await?;
        let mut changed = baseline_fingerprint
            .map(|fingerprint| fingerprint != browser_snapshot_fingerprint(&after))
            .unwrap_or(true);
        if !changed && !self.cancel.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(120)).await;
            after = self.snapshot(tab_id, false, include_values).await?;
            changed = baseline_fingerprint
                .map(|fingerprint| fingerprint != browser_snapshot_fingerprint(&after))
                .unwrap_or(true);
        }
        self.perception.record_snapshot(tab_id, &after);
        self.remember_observation(&after);
        let mut visual_fallback = Value::Null;
        if !changed {
            visual_fallback = self
                .call_native("browser.screenshot", json!({ "tab_id": tab_id }))
                .await
                .unwrap_or(Value::Null);
        }
        Ok(json!({
            "action": action,
            "after": after,
            "observation": if changed { "progress" } else { "no_ax_progress" },
            "visual_fallback": visual_fallback,
        }))
    }

    fn remember_observation(&self, observation: &Value) {
        *self
            .last_observation
            .lock()
            .expect("observation mutex poisoned") = Some(observation.clone());
    }

    async fn call_native(&self, protocol_name: &str, arguments: Value) -> Result<Value, String> {
        let index = self
            .operations
            .lock()
            .expect("operations mutex poisoned")
            .len();
        let call_id = format!(
            "{}:steadwright:{index}:{}",
            self.parent_tool_call_id,
            protocol_name.replace('.', "_")
        );
        let result = self
            .bridge
            .call_browser_tool(
                &call_id,
                protocol_name,
                arguments.clone(),
                self.cancel.clone(),
            )
            .await
            .map_err(|error| error.to_string())?;
        self.operations
            .lock()
            .expect("operations mutex poisoned")
            .push(json!({
                "name": protocol_name,
                "ok": result.ok,
                "tab_id": arguments.get("tab_id").cloned().unwrap_or(Value::Null),
            }));
        if !result.ok {
            return Err(result
                .error
                .unwrap_or_else(|| format!("{protocol_name} failed")));
        }
        if result.tainted {
            return Err(
                "browser result was withheld because the page contains injected credentials"
                    .to_string(),
            );
        }
        let mut content = result.content;
        let mime_type = content
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();
        if let Some(data) = content
            .as_object_mut()
            .and_then(|object| object.remove("image_base64"))
            .and_then(|value| value.as_str().map(str::to_string))
        {
            self.images
                .lock()
                .expect("images mutex poisoned")
                .push(CapturedImage { data, mime_type });
            if let Some(object) = content.as_object_mut() {
                object.insert("image_attached".to_string(), json!(true));
            }
        }
        Ok(content)
    }
}

#[derive(Clone)]
struct CandidateNode {
    reference: Value,
    role: String,
    name: String,
    href: Option<String>,
    bounds: Option<Value>,
    value: Option<String>,
    checked: Option<bool>,
    disabled: bool,
    group: Option<String>,
}

fn matching_nodes(snapshot: &Value, query: &Value) -> Vec<CandidateNode> {
    let mut nodes = Vec::new();
    if let Some(root) = snapshot.pointer("/snapshot/root") {
        collect_candidates(root, None, &mut nodes);
    }
    nodes
        .into_iter()
        .filter(|candidate| locator_matches(candidate, query))
        .collect()
}

fn compatible_action_candidates(
    snapshot: &Value,
    query: &Value,
    operation: &str,
) -> Vec<CandidateNode> {
    if !is_locator_action_operation(operation)
        || query.get("kind").and_then(Value::as_str) != Some("role")
        || query.get("role").and_then(Value::as_str) != Some("button")
    {
        return Vec::new();
    }

    // Models commonly describe visually button-shaped choice cards as
    // buttons even when the page exposes them as radios/checkboxes. If the
    // strict button locator has no match, preserve the requested accessible
    // name and fall back only to checkable controls. This is deterministic,
    // avoids coordinate guessing, and keeps checked-state verification.
    ["radio", "checkbox"]
        .into_iter()
        .flat_map(|role| {
            let mut compatible = query.clone();
            compatible["role"] = Value::String(role.to_string());
            matching_nodes(snapshot, &compatible)
        })
        .collect()
}

fn prioritize_action_candidates(candidates: &mut [CandidateNode], query: &Value, operation: &str) {
    if !is_locator_action_operation(operation) {
        return;
    }

    // Playwright clicks the rendered text and therefore activates its
    // enclosing label/button. AX snapshots also contain leaf static-text
    // nodes, and invoking the AX click action on those leaves can return
    // success without dispatching anything. Prefer the equivalent named
    // interactive node so the action receives normal verification and
    // coordinate fallback behavior.
    if query.get("kind").and_then(Value::as_str) == Some("text") {
        candidates.sort_by_key(|candidate| std::cmp::Reverse(action_candidate_priority(candidate)));
    }
    // Disabled duplicates cannot satisfy an action. Preserve DOM order
    // within each bucket while preferring an actionable match.
    candidates.sort_by_key(|candidate| candidate.disabled);
}

fn is_locator_action_operation(operation: &str) -> bool {
    matches!(
        operation,
        "locator.click"
            | "locator.dblclick"
            | "locator.hover"
            | "locator.check"
            | "locator.uncheck"
            | "locator.focus"
            | "locator.press"
    )
}

fn action_candidate_priority(candidate: &CandidateNode) -> u8 {
    if candidate.checked.is_some() {
        return 3;
    }
    if matches!(
        normalize_role(&candidate.role).as_str(),
        "button" | "link" | "textfield" | "textbox" | "searchbox" | "textarea" | "combobox"
    ) {
        return 2;
    }
    0
}

fn collect_candidates(
    node: &Value,
    inherited_group: Option<&str>,
    output: &mut Vec<CandidateNode>,
) {
    let role = node.get("role").and_then(Value::as_str).unwrap_or("");
    let normalized_role = normalize_role(role);
    let input_group = if matches!(normalized_role.as_str(), "radio" | "radiobutton") {
        node.get("html_input_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(|name| format!("input-name:{name}"))
    } else {
        None
    };
    let accessibility_group = if normalized_role == "radiogroup" {
        node.pointer("/ref/ax_node_id")
            .map(|id| format!("radio-group:{id}"))
            .or_else(|| {
                node.get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(|name| format!("radio-group:{name}"))
            })
    } else {
        None
    };
    let own_group = input_group.or(accessibility_group);
    let group = own_group.as_deref().or(inherited_group);
    if let Some(reference) = node.get("ref") {
        output.push(CandidateNode {
            reference: reference.clone(),
            role: role.to_string(),
            name: node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            href: node
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(str::to_string),
            bounds: node.get("bounds").cloned(),
            value: node
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string),
            checked: node.get("checked").and_then(|value| match value {
                Value::Bool(checked) => Some(*checked),
                Value::String(checked) if checked.eq_ignore_ascii_case("true") => Some(true),
                Value::String(checked) if checked.eq_ignore_ascii_case("false") => Some(false),
                _ => None,
            }),
            disabled: node
                .get("disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            group: group.map(str::to_string),
        });
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            collect_candidates(child, group, output);
        }
    }
}

/// Longest accessible name rendered into a snapshot line.
///
/// Product cards carry marketing copy as their accessible name — one Apple
/// radio names itself with 240 characters of chip description and financing
/// terms. Locator matching still runs against the full name server-side, so
/// truncating the rendered form costs nothing and is most of the tree budget.
const MAX_RENDERED_NAME: usize = 100;

/// One line of a rendered snapshot, before it is joined into text.
struct RenderedLine {
    depth: usize,
    text: String,
}

/// A node earns a line when it is actionable, when it carries state the model
/// needs to see, or when it is a named landmark that gives the tree its shape.
/// Everything else is a generic wrapper and gets collapsed away.
fn is_structural_role(role: &str) -> bool {
    matches!(
        normalize_role(role).as_str(),
        "navigation"
            | "main"
            | "banner"
            | "contentinfo"
            | "complementary"
            | "region"
            | "form"
            | "search"
            | "heading"
            | "dialog"
            | "alertdialog"
            | "alert"
            | "status"
            | "table"
            | "list"
            | "radiogroup"
            | "tablist"
            | "menu"
    )
}

fn is_text_role(role: &str) -> bool {
    matches!(
        normalize_role(role).as_str(),
        "statictext" | "inlinebox" | "paragraph" | "labeltext"
    )
}

fn truncate_name(name: &str) -> String {
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_RENDERED_NAME {
        return collapsed;
    }
    let kept: String = collapsed.chars().take(MAX_RENDERED_NAME).collect();
    format!("{kept}…")
}

/// Render one node's line: `- role "name" [ref=eN] [checked]`.
///
/// Only non-default attributes are emitted. The previous JSON shape wrote
/// `disabled:false, checked:null, value:null, href:null, group:null` on every
/// element whether or not they applied, which was most of the payload.
fn render_line(
    role: &str,
    name: &str,
    handle: Option<&str>,
    node: &Value,
    checked: Option<bool>,
    has_children: bool,
) -> String {
    let normalized = normalize_role(role);
    let mut line = if is_text_role(role) {
        format!("- text: {:?}", truncate_name(name))
    } else if name.is_empty() {
        format!("- {normalized}")
    } else {
        format!("- {normalized} {:?}", truncate_name(name))
    };
    if let Some(handle) = handle {
        line.push_str(&format!(" [ref={handle}]"));
    }
    match checked {
        Some(true) => line.push_str(" [checked]"),
        Some(false) => line.push_str(" [unchecked]"),
        None => {}
    }
    if node
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        line.push_str(" [disabled]");
    }
    if node
        .get("focused")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        line.push_str(" [focused]");
    }
    if let Some(url) = node
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
    {
        line.push_str(&format!(" [url={url}]"));
    }
    if let Some(value) = node
        .get("value")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        line.push_str(&format!(" [value={:?}]", truncate_name(value)));
    }
    if has_children {
        line.push(':');
    }
    line
}

/// Walk the accessibility tree into indented lines, minting a handle for every
/// actionable node.
///
/// Returns whether this subtree emitted anything, so a parent can decide
/// between nesting its children under itself and splicing them in at its own
/// depth — which is what collapses the long chains of anonymous `generic`
/// wrappers that dominate a real page.
#[allow(clippy::too_many_arguments)]
fn render_nodes(
    node: &Value,
    depth: usize,
    inherited_group: Option<&str>,
    parent_name: &str,
    next_handle: &mut usize,
    handles: &mut HashMap<String, (String, String, usize)>,
    seen: &mut HashMap<(String, String), usize>,
    out: &mut Vec<RenderedLine>,
) {
    let role = node.get("role").and_then(Value::as_str).unwrap_or("");
    let name = node.get("name").and_then(Value::as_str).unwrap_or("");
    let normalized_role = normalize_role(role);

    let input_group = if matches!(normalized_role.as_str(), "radio" | "radiobutton") {
        node.get("html_input_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(|name| format!("input-name:{name}"))
    } else {
        None
    };
    let accessibility_group = if normalized_role == "radiogroup" {
        node.pointer("/ref/ax_node_id")
            .map(|id| format!("radio-group:{id}"))
            .or_else(|| (!name.is_empty()).then(|| format!("radio-group:{name}")))
    } else {
        None
    };
    let own_group = input_group.or(accessibility_group);
    let group = own_group.as_deref().or(inherited_group);

    let checked = node.get("checked").and_then(|value| match value {
        Value::Bool(checked) => Some(*checked),
        Value::String(checked) if checked.eq_ignore_ascii_case("true") => Some(true),
        Value::String(checked) if checked.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    });

    let actionable = is_interactive_role(role) || checked.is_some();
    // Static text stays out. Blink fragments running copy into one node per
    // run — a single price renders as "From" / "$949" / "or $42.92" /
    // "per month" — so including it put half the lines in the tree on content
    // the agent cannot act on, and pushed the page past the result cap. What
    // the model needs from a control is already in that control's own line.
    let named_landmark = is_structural_role(role) && !name.is_empty();
    let keep = actionable || named_landmark;

    let mut children = Vec::new();
    let child_depth = if keep { depth + 1 } else { depth };
    let child_parent_name = if name.is_empty() { parent_name } else { name };
    if let Some(list) = node.get("children").and_then(Value::as_array) {
        for child in list {
            render_nodes(
                child,
                child_depth,
                group,
                child_parent_name,
                next_handle,
                handles,
                seen,
                &mut children,
            );
        }
    }

    if keep {
        let handle = actionable.then(|| {
            let handle = format!("e{}", *next_handle);
            *next_handle += 1;
            let identity = (role.to_string(), name.to_string());
            let occurrence = seen.entry(identity).or_insert(0);
            handles.insert(
                handle.clone(),
                (role.to_string(), name.to_string(), *occurrence),
            );
            *occurrence += 1;
            handle
        });
        out.push(RenderedLine {
            depth,
            text: render_line(
                role,
                name,
                handle.as_deref(),
                node,
                checked,
                !children.is_empty(),
            ),
        });
    }
    out.append(&mut children);
}

/// A line-oriented unified diff, in the format models read most fluently.
///
/// After an action the interesting thing is almost never the page — it is the
/// handful of lines that changed. Emitting the delta instead of the tree is
/// what lets the model act repeatedly without re-reading a full snapshot each
/// time.
fn unified_diff(previous: &str, next: &str) -> String {
    let before: Vec<&str> = previous.lines().collect();
    let after: Vec<&str> = next.lines().collect();

    // Longest common subsequence over lines. Snapshots are a few hundred lines,
    // so the quadratic table is far cheaper than the round trip it saves.
    let mut table = vec![vec![0usize; after.len() + 1]; before.len() + 1];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            table[i][j] = if before[i] == after[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    #[derive(PartialEq)]
    enum Edit {
        Same,
        Remove,
        Add,
    }
    let mut edits: Vec<(Edit, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            edits.push((Edit::Same, before[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            edits.push((Edit::Remove, before[i]));
            i += 1;
        } else {
            edits.push((Edit::Add, after[j]));
            j += 1;
        }
    }
    while i < before.len() {
        edits.push((Edit::Remove, before[i]));
        i += 1;
    }
    while j < after.len() {
        edits.push((Edit::Add, after[j]));
        j += 1;
    }

    if edits.iter().all(|(kind, _)| *kind == Edit::Same) {
        return String::new();
    }

    // Group changes into hunks with a little surrounding context, the way a
    // patch does, so each change reads in place.
    const CONTEXT: usize = 2;
    let changed: Vec<usize> = edits
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| *kind != Edit::Same)
        .map(|(index, _)| index)
        .collect();

    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for index in changed {
        let start = index.saturating_sub(CONTEXT);
        let end = (index + CONTEXT + 1).min(edits.len());
        match hunks.last_mut() {
            Some((_, last_end)) if *last_end >= start => *last_end = end,
            _ => hunks.push((start, end)),
        }
    }

    let mut rendered = String::new();
    for (start, end) in hunks {
        let (mut before_line, mut after_line) = (1usize, 1usize);
        for (kind, _) in edits.iter().take(start) {
            match kind {
                Edit::Same => {
                    before_line += 1;
                    after_line += 1;
                }
                Edit::Remove => before_line += 1,
                Edit::Add => after_line += 1,
            }
        }
        let before_count = edits[start..end]
            .iter()
            .filter(|(kind, _)| *kind != Edit::Add)
            .count();
        let after_count = edits[start..end]
            .iter()
            .filter(|(kind, _)| *kind != Edit::Remove)
            .count();
        rendered.push_str(&format!(
            "@@ -{before_line},{before_count} +{after_line},{after_count} @@\n"
        ));
        for (kind, text) in &edits[start..end] {
            let marker = match kind {
                Edit::Same => ' ',
                Edit::Remove => '-',
                Edit::Add => '+',
            };
            rendered.push(marker);
            rendered.push_str(text);
            rendered.push('\n');
        }
    }
    rendered
}

fn locator_matches(candidate: &CandidateNode, query: &Value) -> bool {
    let kind = query.get("kind").and_then(Value::as_str).unwrap_or("css");
    match kind {
        "role" => {
            let requested_role = query.get("role").and_then(Value::as_str).unwrap_or("");
            role_matches(&candidate.role, requested_role)
                && query_text_matches(query, &candidate.name)
                && query
                    .get("group")
                    .and_then(Value::as_str)
                    .is_none_or(|group| candidate.group.as_deref() == Some(group))
                && query
                    .get("checked")
                    .and_then(Value::as_bool)
                    .is_none_or(|checked| candidate.checked == Some(checked))
        }
        "text" => query_has_text_constraint(query) && query_text_matches(query, &candidate.name),
        "label" | "placeholder" => {
            is_editable_role(&candidate.role)
                && query_has_text_constraint(query)
                && query_text_matches(query, &candidate.name)
        }
        "title" | "alt" | "testid" => {
            query_has_text_constraint(query) && query_text_matches(query, &candidate.name)
        }
        "css" => css_matches(
            candidate,
            query.get("selector").and_then(Value::as_str).unwrap_or(""),
        ),
        _ => false,
    }
}

fn query_has_text_constraint(query: &Value) -> bool {
    query_name(query).is_some() || query.get("name_regex").and_then(Value::as_str).is_some()
}

fn query_text_matches(query: &Value, actual: &str) -> bool {
    let positive = if let Some(pattern) = query.get("name_regex").and_then(Value::as_str) {
        let flags = query
            .get("name_regex_flags")
            .and_then(Value::as_str)
            .unwrap_or("");
        regex::RegexBuilder::new(pattern)
            .case_insensitive(flags.contains('i'))
            .multi_line(flags.contains('m'))
            .dot_matches_new_line(flags.contains('s'))
            .build()
            .is_ok_and(|regex| regex.is_match(actual))
    } else {
        let exact = query.get("exact").and_then(Value::as_bool).unwrap_or(false);
        query_name(query).is_none_or(|name| text_matches(actual, name, exact))
    };
    if !positive {
        return false;
    }
    if let Some(pattern) = query.get("name_not_regex").and_then(Value::as_str) {
        if regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(actual)) {
            return false;
        }
    }
    query
        .get("name_not")
        .and_then(Value::as_str)
        .is_none_or(|name| !text_matches(actual, name, false))
}

fn query_name(query: &Value) -> Option<&str> {
    query
        .get("name")
        .or_else(|| query.get("text"))
        .and_then(Value::as_str)
}

fn text_matches(actual: &str, requested: &str, exact: bool) -> bool {
    if exact {
        actual.eq_ignore_ascii_case(requested)
    } else {
        actual.to_lowercase().contains(&requested.to_lowercase())
    }
}

fn normalize_role(role: &str) -> String {
    role.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn role_matches(actual: &str, requested: &str) -> bool {
    let actual = normalize_role(actual);
    let requested = normalize_role(requested);
    actual == requested
        || matches!(
            (actual.as_str(), requested.as_str()),
            ("radiobutton", "radio")
                | ("checkbox", "check")
                | ("textfield", "textbox")
                | ("searchbox", "textbox")
                | ("statictext", "text")
                | ("inlinebox", "text")
                | ("popupbutton", "combobox")
        )
}

fn is_editable_role(role: &str) -> bool {
    matches!(
        normalize_role(role).as_str(),
        "textfield" | "textbox" | "searchbox" | "textarea" | "combobox"
    )
}

fn css_matches(candidate: &CandidateNode, selector: &str) -> bool {
    let selector = selector.trim();
    if selector.contains(',') {
        return selector
            .split(',')
            .any(|part| css_matches(candidate, part.trim()));
    }
    if let Some(text) = selector.strip_prefix("text=") {
        return text_matches(&candidate.name, text.trim_matches(['\'', '"']), false);
    }
    if let Some(role) = extract_attribute(selector, "role") {
        if !role_matches(&candidate.role, role) {
            return false;
        }
    }
    if let Some(label) = extract_attribute(selector, "aria-label") {
        if !text_matches(&candidate.name, label, true) {
            return false;
        }
    }
    let tag = selector
        .split(['[', '.', '#', ':', ' ', '>'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if tag.is_empty() || tag == "*" || selector.starts_with('[') {
        return true;
    }
    match tag.as_str() {
        "button" => role_matches(&candidate.role, "button"),
        "a" => role_matches(&candidate.role, "link"),
        "input" => match extract_attribute(selector, "type")
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "radio" => role_matches(&candidate.role, "radio"),
            "checkbox" => role_matches(&candidate.role, "checkbox"),
            _ => {
                is_editable_role(&candidate.role)
                    || role_matches(&candidate.role, "radio")
                    || role_matches(&candidate.role, "checkbox")
            }
        },
        "textarea" => is_editable_role(&candidate.role),
        "select" => role_matches(&candidate.role, "combobox"),
        "img" => role_matches(&candidate.role, "image"),
        "body" | "html" => matches!(
            normalize_role(&candidate.role).as_str(),
            "rootwebarea" | "webarea" | "document"
        ),
        "label" => matches!(
            normalize_role(&candidate.role).as_str(),
            "statictext" | "text"
        ),
        "div" => extract_attribute(selector, "role")
            .is_some_and(|role| role_matches(&candidate.role, role)),
        _ => false,
    }
}

fn extract_attribute<'a>(selector: &'a str, attribute: &str) -> Option<&'a str> {
    let marker = format!("[{attribute}=");
    let start = selector.find(&marker)? + marker.len();
    let rest = &selector[start..];
    let end = rest.find(']')?;
    Some(rest[..end].trim().trim_matches(['\'', '"']))
}

fn required_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn required_positive_i32(value: &Value, key: &str) -> Result<i32, String> {
    positive_i32(value.get(key)).ok_or_else(|| format!("{key} must be a positive integer"))
}

fn positive_i32(value: Option<&Value>) -> Option<i32> {
    value
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value > 0)
}

fn descriptor_tab_id(
    descriptor: &Value,
    default_tab_id: &StdMutex<Option<i32>>,
) -> Result<i32, String> {
    positive_i32(descriptor.get("tab_id"))
        .or_else(|| {
            default_tab_id
                .lock()
                .expect("tab mutex poisoned")
                .filter(|tab_id| *tab_id > 0)
        })
        .ok_or_else(|| "locator has no browser tab".to_string())
}

fn resolve_descriptor(snapshot: &Value, descriptor: &Value) -> Result<Value, String> {
    let query = descriptor
        .get("query")
        .ok_or_else(|| "locator query is missing".to_string())?;
    let index = descriptor.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let candidates = matching_nodes(snapshot, query);
    candidates
        .get(index)
        .map(|candidate| candidate.reference.clone())
        .ok_or_else(|| {
            format!(
                "credential locator matched {} elements, so index {index} is unavailable",
                candidates.len()
            )
        })
}

/// Bounded polling loop shared by every wait operation.
///
/// The first probe always runs before the first sleep, so a condition that is
/// already satisfied costs nothing — Playwright behaves the same way, and it is
/// what lets the model treat an already-satisfied postcondition as done work.
struct WaitLoop {
    deadline: Instant,
    interval: Duration,
}

impl WaitLoop {
    fn new(timeout: Duration, interval: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            interval,
        }
    }

    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
    }

    /// Sleep until the next probe. Returns false once the deadline has passed,
    /// which ends the loop and leaves the caller to raise the timeout error.
    async fn tick(&self) -> bool {
        let Some(remaining) = self.remaining() else {
            return false;
        };
        tokio::time::sleep(self.interval.min(remaining)).await;
        true
    }
}

/// Read a Playwright-style `options.timeout` in milliseconds. `0` disables the
/// timeout in Playwright; here it clamps to MAX_WAIT_TIMEOUT so a runaway wait
/// can never outlive BROWSER_EXEC_TIMEOUT.
fn wait_timeout(args: &Value, fallback: Duration) -> Duration {
    match args
        .pointer("/options/timeout")
        .or_else(|| args.get("timeout"))
        .and_then(Value::as_u64)
    {
        Some(0) => MAX_WAIT_TIMEOUT,
        Some(ms) => Duration::from_millis(ms).min(MAX_WAIT_TIMEOUT),
        None => fallback,
    }
}

fn wait_poll_interval(args: &Value) -> Duration {
    args.pointer("/options/polling")
        .and_then(Value::as_u64)
        .filter(|polling| *polling > 0)
        .map(Duration::from_millis)
        .unwrap_or(WAIT_POLL_INTERVAL)
        .min(MAX_WAIT_TIMEOUT)
}

/// Translate a Playwright URL glob into a regex.
///
/// `**` crosses path separators, `*` does not, and `?` matches one character —
/// the same shapes Playwright documents, so a pattern the model already knows
/// behaves the way it expects.
fn url_glob_to_regex(glob: &str) -> String {
    let characters: Vec<char> = glob.chars().collect();
    let mut pattern = String::from("^");
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '*' => {
                if characters.get(index + 1) == Some(&'*') {
                    pattern.push_str(".*");
                    index += 2;
                    continue;
                }
                pattern.push_str("[^/]*");
            }
            '?' => pattern.push('.'),
            character => pattern.push_str(&regex::escape(&character.to_string())),
        }
        index += 1;
    }
    pattern.push('$');
    pattern
}

/// Match a URL against the normalized pattern the JS surface sends: a RegExp
/// becomes `{regex, flags}`, a string becomes `{glob}`.
fn url_pattern_matches(pattern: &Value, url: &str) -> bool {
    if let Some(source) = pattern.get("regex").and_then(Value::as_str) {
        let flags = pattern
            .get("flags")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return regex::RegexBuilder::new(source)
            .case_insensitive(flags.contains('i'))
            .multi_line(flags.contains('m'))
            .dot_matches_new_line(flags.contains('s'))
            .build()
            .is_ok_and(|regex| regex.is_match(url));
    }
    if let Some(glob) = pattern.get("glob").and_then(Value::as_str) {
        if glob == url {
            return true;
        }
        return regex::Regex::new(&url_glob_to_regex(glob)).is_ok_and(|regex| regex.is_match(url));
    }
    false
}

fn describe_url_pattern(pattern: &Value) -> String {
    if let Some(source) = pattern.get("regex").and_then(Value::as_str) {
        return format!("/{source}/");
    }
    pattern
        .get("glob")
        .and_then(Value::as_str)
        .unwrap_or("<pattern>")
        .to_string()
}

/// Playwright's four `waitForSelector` / `locator.waitFor` states.
///
/// Stead resolves locators against the accessibility tree, which already omits
/// nodes that are not exposed to assistive technology. `attached` and `visible`
/// therefore collapse onto "the node is in the tree", and `detached` and
/// `hidden` onto "it is not". The distinction Playwright draws between them —
/// an attached-but-`visibility:hidden` node — is not observable here, and
/// pretending otherwise would be worse than saying so.
fn wait_state_expects_presence(state: &str) -> Result<bool, String> {
    match state {
        "attached" | "visible" => Ok(true),
        "detached" | "hidden" => Ok(false),
        other => Err(format!(
            "unsupported wait state {other:?}; use attached, detached, visible, or hidden"
        )),
    }
}

/// Stead observes the network through `ResourceLoadComplete`, which fires once
/// a resource has finished. A record therefore describes a completed exchange
/// and carries both the request and the response fields; `kind` selects which
/// view the caller asked for rather than which moment was observed.
fn network_record_is_kind(record: &Value, kind: &str) -> bool {
    match kind {
        "request" | "response" => record
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| !url.is_empty()),
        _ => false,
    }
}

/// Name the elements an ambiguous locator hit. A strict-mode failure that only
/// reports a count leaves the model guessing; one that lists what it found
/// usually tells it exactly how to narrow.
fn candidate_summary(candidates: &[CandidateNode]) -> String {
    const SHOWN: usize = 4;
    let mut described: Vec<String> = candidates
        .iter()
        .take(SHOWN)
        .map(|candidate| {
            let role = if candidate.role.is_empty() {
                "element"
            } else {
                candidate.role.as_str()
            };
            if candidate.name.is_empty() {
                format!("{role} (unnamed)")
            } else {
                format!("{role} {:?}", candidate.name)
            }
        })
        .collect();
    if candidates.len() > SHOWN {
        described.push(format!("and {} more", candidates.len() - SHOWN));
    }
    described.join(", ")
}

/// JavaScript truthiness, so `waitForFunction` accepts the same predicates the
/// model would write for Playwright (`() => document.querySelector('.done')`).
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn url_origin(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid page URL: {error}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "page URL has no origin".to_string())?;
    let mut origin = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Ok(origin)
}

fn navigation_target_reached(actual: &str, expected: &str) -> bool {
    if actual.trim_end_matches('/') == expected.trim_end_matches('/') {
        return true;
    }
    let (Ok(actual), Ok(expected)) = (reqwest::Url::parse(actual), reqwest::Url::parse(expected))
    else {
        return false;
    };
    actual.scheme() == expected.scheme()
        && actual.host_str() == expected.host_str()
        && actual.port_or_known_default() == expected.port_or_known_default()
        && actual.path().trim_end_matches('/') == expected.path().trim_end_matches('/')
}

fn resolved_link_target(snapshot: &Value, candidate: &CandidateNode) -> Option<String> {
    let href = candidate.href.as_deref()?.trim();
    let parsed = reqwest::Url::parse(href).ok().or_else(|| {
        let base = snapshot.pointer("/snapshot/url").and_then(Value::as_str)?;
        reqwest::Url::parse(base).ok()?.join(href).ok()
    })?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.to_string())
}

fn value_tab_id(value: &Value) -> Option<i32> {
    positive_i32(value.get("tab_id"))
}

fn is_missing_tab_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("tab was not found")
        || normalized.contains("tab not found")
        || normalized.contains("no such tab")
}

fn is_transient_snapshot_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("snapshot was unavailable")
        || normalized.contains("snapshot is unavailable")
        || normalized.contains("frame-local accessibility snapshot")
        || normalized.contains("render frame")
        || normalized.contains("page is loading")
}

fn is_stale_snapshot_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("old snapshot")
        || normalized.contains("stale snapshot")
        || normalized.contains("not present in the latest snapshot")
        || normalized.contains("not present in the current ax tree")
}

fn point_from_value(value: Option<&Value>) -> Result<Value, String> {
    let value = value.ok_or_else(|| "point is missing".to_string())?;
    let x = value
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| "point.x is missing".to_string())?;
    let y = value
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| "point.y is missing".to_string())?;
    Ok(json!({ "x": x.round() as i64, "y": y.round() as i64 }))
}

fn bounds_center(bounds: &Value) -> Result<Value, String> {
    let x = bounds
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| "bounds.x is missing".to_string())?;
    let y = bounds
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| "bounds.y is missing".to_string())?;
    let width = bounds
        .get("width")
        .and_then(Value::as_f64)
        .ok_or_else(|| "bounds.width is missing".to_string())?;
    let height = bounds
        .get("height")
        .and_then(Value::as_f64)
        .ok_or_else(|| "bounds.height is missing".to_string())?;
    Ok(json!({
        "x": (x + width / 2.0).round() as i64,
        "y": (y + height / 2.0).round() as i64,
    }))
}

fn usable_bounds(bounds: &Value) -> bool {
    bounds
        .get("width")
        .and_then(Value::as_f64)
        .is_some_and(|width| width > 0.0)
        && bounds
            .get("height")
            .and_then(Value::as_f64)
            .is_some_and(|height| height > 0.0)
}

fn probe_requires_wheel_scroll(probe: &Value) -> bool {
    let Some(bounds) = probe.pointer("/probe/client_rect") else {
        return false;
    };
    if !usable_bounds(bounds) {
        return false;
    }
    probe
        .pointer("/probe/hit_test_stack")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
}

fn adjusted_point_for_viewport(point: &Value, error: &str) -> Option<Value> {
    let dimensions = error
        .split("outside the ")
        .nth(1)?
        .split(" visible viewport")
        .next()?;
    let (width, height) = dimensions.split_once('x')?;
    let width = width.trim().parse::<f64>().ok()?;
    let height = height.trim().parse::<f64>().ok()?;
    let x = point.get("x")?.as_f64()?;
    let y = point.get("y")?.as_f64()?;
    if width <= 0.0 || height <= 0.0 || x < 0.0 || y < 0.0 {
        return None;
    }
    let required_scale = (x / width).max(y / height);
    let scale = [1.25_f64, 1.5, 2.0, 3.0, 4.0]
        .into_iter()
        .find(|scale| *scale >= required_scale && x / scale < width && y / scale < height)?;
    Some(json!({ "x": x / scale, "y": y / scale }))
}

fn mouse_button(options: &Value) -> i32 {
    match options
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
    {
        "middle" => 1,
        "right" => 2,
        _ => 0,
    }
}

fn parse_key_combo(combo: &str) -> (String, i32) {
    let mut parts = combo.split('+').collect::<Vec<_>>();
    let key = parts.pop().unwrap_or(combo).to_string();
    let mut modifiers = 0;
    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "shift" => modifiers |= 1,
            "control" | "ctrl" => modifiers |= 2,
            "alt" | "option" => modifiers |= 4,
            "meta" | "command" | "cmd" => modifiers |= 8,
            _ => {}
        }
    }
    (key, modifiers)
}

fn parse_eval_result(result: Value) -> Value {
    let Some(encoded) = result.get("json_result").and_then(Value::as_str) else {
        return result;
    };
    serde_json::from_str(encoded).unwrap_or_else(|_| json!(encoded))
}

pub(crate) struct BrowserCodeTool {
    definition: pie_ai::Tool,
    session_id: String,
    bridge: Arc<dyn BrowserToolBridge>,
    perception: Arc<BrowserPerceptionState>,
    runtimes: Arc<BrowserRuntimePool>,
    consecutive_failures: AtomicUsize,
}

impl BrowserCodeTool {
    pub(crate) fn new(
        session_id: String,
        bridge: Arc<dyn BrowserToolBridge>,
        perception: Arc<BrowserPerceptionState>,
        runtimes: Arc<BrowserRuntimePool>,
    ) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: "browser_exec".to_string(),
                description: "Execute Playwright-compatible JavaScript in Stead's persistent browser REPL. Globals: page, context, browser, state, display, help. Only `state` persists between executions; lexical variables do not. Semantic locators, state-based waits (waitForURL, waitForSelector, waitForFunction, waitForLoadState, waitForResponse, waitForRequest), screenshots, and page.evaluate are available; call help('page') or help('locator') inside an execution for exact signatures. Native Rust/Chromium operations are individually audited, cancellable, and automatically observed. Read the browser-automation skill before non-trivial tasks — it covers batching a whole task into one execution, waiting on state instead of sleeping, and recovering from failures."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["code"],
                    "properties": {
                        "code": { "type": "string", "description": "Async JavaScript body. Top-level await and return are supported." },
                        "tab_id": { "type": "integer", "minimum": 1, "description": "Optional current Stead tab id. Omit it when no browser tab is attached; page.goto() will create an agent-owned tab." }
                    }
                }),
            },
            session_id,
            bridge,
            perception,
            runtimes,
            consecutive_failures: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AgentTool for BrowserCodeTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        "browser_exec"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    fn permission_classification(&self, _prepared_args: &Value) -> PermissionClassification {
        PermissionClassification::Allow
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let raw_code = params
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::Message("browser_exec requires code".to_string()))?;
        if raw_code.len() > MAX_BROWSER_EXEC_CODE_BYTES {
            return Err(AgentToolError::Message(format!(
                "browser_exec code exceeds the {MAX_BROWSER_EXEC_CODE_BYTES}-byte limit"
            )));
        }
        let code = normalize_browser_exec_code(raw_code);
        if code.is_empty() {
            return Err(AgentToolError::Message(
                "browser_exec code was empty after removing malformed transcript text".to_string(),
            ));
        }
        let default_tab_id = params
            .get("tab_id")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .filter(|value| *value > 0);
        let runtime = self
            .runtimes
            .runtime_for(&self.session_id)
            .await
            .map_err(AgentToolError::Message)?;
        let outcome = match runtime
            .execute(
                &code,
                default_tab_id,
                self.bridge.clone(),
                self.perception.clone(),
                tool_call_id,
                cancel,
            )
            .await
        {
            Ok(outcome) => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
                outcome
            }
            Err(error) => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= MAX_CONSECUTIVE_BROWSER_EXEC_FAILURES {
                    let message = format!(
                        "Browser execution stopped after {failures} consecutive failures: {error}"
                    );
                    let details = json!({
                        "status": "browser_execution_stopped",
                        "consecutive_failures": failures,
                        "error": error,
                    });
                    return Ok(AgentToolResult {
                        content: vec![pie_ai::UserContentBlock::text(format!(
                            "{message}. Do not call browser_exec again in this turn; explain the concrete browser failure to the user."
                        ))],
                        details,
                        // Let the model produce a user-facing failure instead
                        // of ending the turn with the generic "no answer"
                        // fallback. Further browser calls remain guarded by
                        // the consecutive-failure counter.
                        terminate: None,
                    });
                }
                return Err(AgentToolError::Message(error));
            }
        };

        let raw_result_value = outcome.result.get("value").cloned().unwrap_or(Value::Null);
        let result_value = compact_browser_exec_value(raw_result_value.clone());
        let last_observation = outcome
            .last_observation
            .filter(|observation| {
                raw_result_value != *observation
                    && raw_result_value.get("after") != Some(observation)
            })
            .map(|observation| {
                let compact = compact_browser_observation(&observation);
                let tab_id = compact.get("tab_id").and_then(Value::as_i64).unwrap_or(0) as i32;
                let previous = self
                    .perception
                    .exchange_compact_observation(tab_id, compact.clone());
                previous
                    .as_ref()
                    .and_then(|previous| diff_browser_observation(previous, &compact))
                    .unwrap_or(compact)
            });
        let logs = compact_browser_exec_logs(
            outcome
                .result
                .get("logs")
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
        let summary = json!({
            "result": result_value,
            "logs": logs,
            "operations": outcome.operations,
            "last_observation": last_observation,
            "screenshots_attached": outcome.images.len(),
        });
        let mut content = vec![pie_ai::UserContentBlock::text(summary.to_string())];
        let mut seen_images = HashSet::new();
        for image in outcome.images {
            if !seen_images.insert(image.data.clone()) {
                continue;
            }
            content.push(pie_ai::UserContentBlock::Image(pie_ai::ImageContent {
                data: image.data,
                mime_type: image.mime_type,
            }));
        }
        Ok(AgentToolResult {
            content,
            details: summary,
            terminate: None,
        })
    }
}

fn compact_browser_exec_logs(logs: Value) -> Value {
    let mut remaining = MAX_BROWSER_EXEC_LOG_BYTES;
    let mut compact = Vec::new();
    for log in logs.as_array().into_iter().flatten() {
        if remaining == 0 {
            break;
        }
        let text = log
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| log.to_string());
        let original_bytes = text.len();
        let take = remaining.min(original_bytes);
        let mut end = take;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bounded = text[..end].to_string();
        if end < original_bytes {
            bounded.push_str(&format!("\n[log truncated from {original_bytes} bytes]"));
        }
        remaining = remaining.saturating_sub(bounded.len());
        compact.push(Value::String(bounded));
    }
    Value::Array(compact)
}

fn truncated_value_preview(value: &Value) -> Value {
    let serialized = value.to_string();
    let mut end = MAX_BROWSER_EXEC_RESULT_BYTES.min(serialized.len());
    while end > 0 && !serialized.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "stead_truncated": true,
        "original_bytes": serialized.len(),
        "preview": &serialized[..end],
    })
}

fn compact_browser_exec_value(value: Value) -> Value {
    if value.to_string().len() <= MAX_BROWSER_EXEC_RESULT_BYTES {
        return value;
    }
    if value.get("snapshot").is_some() || value.pointer("/after/snapshot").is_some() {
        return compact_browser_observation(&value);
    }
    truncated_value_preview(&value)
}

fn compact_browser_observation(observation: &Value) -> Value {
    let snapshot = observation
        .get("snapshot")
        .or_else(|| observation.pointer("/after/snapshot"))
        .or_else(|| observation.pointer("/after"));
    let Some(snapshot) = snapshot else {
        return truncated_value_preview(observation);
    };
    let mut candidates = Vec::new();
    if let Some(root) = snapshot.get("root") {
        collect_candidates(root, None, &mut candidates);
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(compact_candidate_priority(candidate)));
    let elements = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.disabled
                || candidate.checked.is_some()
                || matches!(
                    normalize_role(&candidate.role).as_str(),
                    "button"
                        | "link"
                        | "radiobutton"
                        | "checkbox"
                        | "textfield"
                        | "textbox"
                        | "searchbox"
                        | "combobox"
                        | "heading"
                        | "group"
                )
        })
        .take(MAX_COMPACT_OBSERVATION_NODES)
        .map(|candidate| {
            json!({
                "role": candidate.role,
                "name": candidate.name,
                "disabled": candidate.disabled,
                "checked": candidate.checked,
                "value": candidate.value,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "tab_id": snapshot.get("tab_id").cloned().unwrap_or(Value::Null),
        "url": snapshot.get("url").cloned().unwrap_or(Value::Null),
        "title": snapshot.get("title").cloned().unwrap_or(Value::Null),
        "generation": snapshot.get("generation").cloned().unwrap_or(Value::Null),
        "node_count": snapshot.get("node_count").cloned().unwrap_or(Value::Null),
        "truncated": snapshot.get("truncated").cloned().unwrap_or(Value::Null),
        "elements": elements,
    })
}

/// Report an observation as the change it represents rather than the page it
/// describes.
///
/// After a click, the model's real question is "what did that do?" — sending a
/// fresh 32-element tree makes it re-derive the answer every step, and pays for
/// the whole page to do it. An element is keyed by role plus accessible name,
/// so a control that merely became enabled or checked shows up as `updated`
/// with its old and new state instead of as an unrelated add and remove.
///
/// Falls back to the full observation when nothing changed structurally or when
/// the page navigated, since a diff across documents is noise.
fn diff_browser_observation(previous: &Value, next: &Value) -> Option<Value> {
    let key = |element: &Value| {
        format!(
            "{}\u{1}{}",
            element.get("role").and_then(Value::as_str).unwrap_or(""),
            element.get("name").and_then(Value::as_str).unwrap_or(""),
        )
    };
    if previous.get("url") != next.get("url") {
        return None;
    }
    let previous_elements = previous.get("elements")?.as_array()?;
    let next_elements = next.get("elements")?.as_array()?;
    let previous_by_key: HashMap<String, &Value> = previous_elements
        .iter()
        .map(|element| (key(element), element))
        .collect();
    let next_by_key: HashMap<String, &Value> = next_elements
        .iter()
        .map(|element| (key(element), element))
        .collect();

    let mut added = Vec::new();
    let mut updated = Vec::new();
    for element in next_elements {
        match previous_by_key.get(&key(element)) {
            None => added.push(element.clone()),
            Some(before) if *before != element => updated.push(json!({
                "role": element.get("role").cloned().unwrap_or(Value::Null),
                "name": element.get("name").cloned().unwrap_or(Value::Null),
                "from": {
                    "disabled": before.get("disabled").cloned().unwrap_or(Value::Null),
                    "checked": before.get("checked").cloned().unwrap_or(Value::Null),
                    "value": before.get("value").cloned().unwrap_or(Value::Null),
                },
                "to": {
                    "disabled": element.get("disabled").cloned().unwrap_or(Value::Null),
                    "checked": element.get("checked").cloned().unwrap_or(Value::Null),
                    "value": element.get("value").cloned().unwrap_or(Value::Null),
                },
            })),
            Some(_) => {}
        }
    }
    let removed: Vec<Value> = previous_elements
        .iter()
        .filter(|element| !next_by_key.contains_key(&key(element)))
        .cloned()
        .collect();

    if added.is_empty() && updated.is_empty() && removed.is_empty() {
        return Some(json!({
            "url": next.get("url").cloned().unwrap_or(Value::Null),
            "unchanged": true,
            "elements_seen": next_elements.len(),
        }));
    }
    Some(json!({
        "url": next.get("url").cloned().unwrap_or(Value::Null),
        "title": next.get("title").cloned().unwrap_or(Value::Null),
        "added": added,
        "updated": updated,
        "removed": removed,
        "unchanged_count": next_elements.len() - added.len() - updated.len(),
    }))
}

/// Roles a model can actually act on. Everything else is page furniture.
fn is_interactive_role(role: &str) -> bool {
    matches!(
        normalize_role(role).as_str(),
        "button"
            | "link"
            | "radiobutton"
            | "checkbox"
            | "textfield"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "listbox"
            | "menuitem"
            | "option"
            | "switch"
            | "slider"
            | "tab"
    )
}

fn compact_candidate_priority(candidate: &CandidateNode) -> u16 {
    let role = normalize_role(&candidate.role);
    let name = candidate.name.to_ascii_lowercase();
    let mut priority = match role.as_str() {
        "radiobutton" | "checkbox" | "textfield" | "textbox" | "searchbox" | "combobox" => 100,
        "button" => 85,
        "group" => 75,
        "heading" => 70,
        "link" => 40,
        _ => 0,
    };
    if role == "link"
        && ["buy", "configure", "continue", "review", "add to bag"]
            .iter()
            .any(|term| name.contains(term))
    {
        priority += 60;
    }
    if candidate.checked.is_some() {
        priority += 30;
    }
    if candidate.disabled {
        priority += 10;
    }
    priority
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn browser_exec_discards_accidentally_appended_tool_transcript() {
        let code = "await page.goto('https://www.apple.com/ca/store'); return {url: page.url()};} stray assistant to=functions.browser_exec code";
        assert_eq!(
            normalize_browser_exec_code(code),
            "await page.goto('https://www.apple.com/ca/store'); return {url: page.url()};"
        );
    }

    #[test]
    fn browser_exec_preserves_normal_javascript() {
        let code = "const label = 'assistant';\nreturn {label};";
        assert_eq!(normalize_browser_exec_code(code), code);
    }
    use stead_brain_protocol::ToolResultPayload;

    #[test]
    fn radio_candidates_inherit_their_accessibility_group() {
        let snapshot = json!({
            "snapshot": {
                "root": {
                    "role": "rootWebArea",
                    "children": [{
                        "ref": {"ax_node_id": 10},
                        "role": "radioGroup",
                        "name": "Memory",
                        "children": [{
                            "ref": {"ax_node_id": 11},
                            "role": "radioButton",
                            "name": "16GB",
                            "checked": true,
                            "disabled": false,
                            "children": []
                        }, {
                            "ref": {"ax_node_id": 12},
                            "role": "radioButton",
                            "name": "24GB",
                            "checked": false,
                            "disabled": false,
                            "children": []
                        }]
                    }]
                }
            }
        });
        let radios = matching_nodes(&snapshot, &json!({"kind": "role", "role": "radio"}));

        assert_eq!(radios.len(), 2);
        assert_eq!(radios[0].group.as_deref(), Some("radio-group:10"));
        assert_eq!(radios[1].group, radios[0].group);
    }

    /// A tree shaped like the Apple configurator: a named landmark, a wrapper
    /// chain carrying no semantics, a radio whose accessible name is marketing
    /// copy, and a disabled action.
    fn configurator_tree() -> Value {
        json!({
            "snapshot": {
                "url": "https://www.apple.com/ca/shop/buy-mac/mac-mini",
                "title": "Buy Mac mini - Apple (CA)",
                "root": {
                    "role": "rootWebArea",
                    "children": [
                        {
                            "role": "navigation",
                            "name": "Global",
                            "children": [{
                                "ref": {"ax_node_id": 2},
                                "role": "link",
                                "name": "Apple",
                                "url": "https://www.apple.com/ca/",
                                "children": []
                            }]
                        },
                        {
                            "role": "genericContainer",
                            "name": "",
                            "children": [{
                                "role": "genericContainer",
                                "name": "",
                                "children": [{
                                    "ref": {"ax_node_id": 35},
                                    "role": "radioButton",
                                    "name": "M4 chip Tremendous performance to help you tackle almost anything you put your mind to. 10-core CPU, 10-core GPU, 16-core Neural Engine From $1099 or $49.70 per month for 24 months at 7.99% APR",
                                    "html_input_name": "processor",
                                    "checked": "true",
                                    "children": []
                                }]
                            }]
                        },
                        {
                            "ref": {"ax_node_id": 60},
                            "role": "button",
                            "name": "Continue",
                            "disabled": true,
                            "children": []
                        }
                    ]
                }
            }
        })
    }

    fn render(snapshot: &Value) -> String {
        let mut handles = HashMap::new();
        let mut seen = HashMap::new();
        let mut lines = Vec::new();
        let mut next = 1usize;
        render_nodes(
            snapshot.pointer("/snapshot/root").expect("root"),
            0,
            None,
            "",
            &mut next,
            &mut handles,
            &mut seen,
            &mut lines,
        );
        lines
            .iter()
            .map(|line| format!("{}{}", "  ".repeat(line.depth), line.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn snapshot_lines_omit_attributes_that_do_not_apply() {
        let text = render(&configurator_tree());

        // The old JSON shape spent `disabled:false,checked:null,value:null,
        // href:null,group:null` on every element regardless of relevance.
        assert!(!text.contains("value="), "{text}");
        assert!(!text.contains("[unchecked]"), "{text}");
        assert!(
            text.contains(r#"- link "Apple" [ref=e1] [url=https://www.apple.com/ca/]"#),
            "{text}"
        );
        assert!(text.contains("[checked]"), "{text}");
        assert!(
            text.contains(r#"- button "Continue" [ref=e3] [disabled]"#),
            "{text}"
        );
    }

    #[test]
    fn snapshot_collapses_anonymous_wrappers_but_keeps_named_landmarks() {
        let text = render(&configurator_tree());
        let lines: Vec<&str> = text.lines().collect();

        // The named landmark survives and nests its link.
        assert_eq!(lines[0], r#"- navigation "Global":"#);
        assert!(lines[1].starts_with("  - link"), "{text}");
        // Two anonymous genericContainer wrappers collapse: the radio lands at
        // depth 0, not depth 2.
        let radio = lines
            .iter()
            .find(|line| line.contains("radiobutton"))
            .expect("radio");
        assert!(!radio.starts_with(' '), "{radio}");
        assert!(!text.contains("genericcontainer"), "{text}");
    }

    #[test]
    fn snapshot_truncates_marketing_copy_used_as_an_accessible_name() {
        let text = render(&configurator_tree());
        let radio = text
            .lines()
            .find(|line| line.contains("radiobutton"))
            .expect("radio");

        assert!(radio.contains("M4 chip Tremendous performance"), "{radio}");
        assert!(radio.contains('…'), "{radio}");
        // Locator matching still runs against the untruncated name, so this is
        // purely a rendering budget.
        let radios = matching_nodes(
            &configurator_tree(),
            &json!({"kind": "role", "role": "radio", "name": "M4 chip Tremendous"}),
        );
        assert_eq!(radios.len(), 1);
        assert_eq!(radios[0].checked, Some(true));
    }

    #[test]
    fn diff_reports_only_the_lines_that_changed() {
        // A configurator page is mostly chrome that never moves; selecting an
        // option changes two lines out of dozens. That ratio is the whole
        // point of sending a diff instead of the tree.
        let page = |memory_checked: bool, continue_enabled: bool| {
            let mut text = String::new();
            for index in 1..=20 {
                text.push_str(&format!("- link \"Nav {index}\" [ref=n{index}]\n"));
            }
            text.push_str(&format!(
                "- radiobutton \"16GB\" [ref=e1] [{}]\n",
                if memory_checked {
                    "checked"
                } else {
                    "unchecked"
                }
            ));
            text.push_str(&format!(
                "- button \"Continue\" [ref=e2]{}\n",
                if continue_enabled { "" } else { " [disabled]" }
            ));
            text
        };
        let before = page(false, false);
        let after = page(true, true);
        let diff = unified_diff(&before, &after);

        assert!(diff.contains("@@"), "{diff}");
        assert!(
            diff.contains("-- radiobutton \"16GB\" [ref=e1] [unchecked]"),
            "{diff}"
        );
        assert!(
            diff.contains("+- radiobutton \"16GB\" [ref=e1] [checked]"),
            "{diff}"
        );
        // The twenty untouched nav links must not be reprinted.
        assert!(!diff.contains("Nav 1\""), "{diff}");
        assert!(diff.len() < after.len() / 2, "{diff}");
    }

    #[test]
    fn element_roles_keep_their_accessibility_casing() {
        // A normalized role here fails silently rather than loudly: `'button'`
        // survives lowercasing and keeps matching, so only the radio filters
        // come back empty and the model reads that as "no radios on the page".
        let mut candidates = Vec::new();
        collect_candidates(
            configurator_tree().pointer("/snapshot/root").expect("root"),
            None,
            &mut candidates,
        );
        let roles: Vec<&str> = candidates
            .iter()
            .filter(|candidate| is_interactive_role(&candidate.role))
            .map(|candidate| candidate.role.as_str())
            .collect();

        assert!(roles.contains(&"radioButton"), "{roles:?}");
        assert!(!roles.contains(&"radiobutton"), "{roles:?}");
    }

    #[test]
    fn snapshot_text_leaves_out_fragmented_static_text() {
        // Blink splits running copy into one node per run, so a single price
        // arrives as four separate text nodes.
        let tree = json!({
            "snapshot": {
                "root": {
                    "role": "rootWebArea",
                    "children": [
                        {"role": "staticText", "name": "From", "children": []},
                        {"role": "staticText", "name": "$949", "children": []},
                        {"role": "staticText", "name": "or $42.92", "children": []},
                        {"role": "staticText", "name": "per month", "children": []},
                        {
                            "ref": {"ax_node_id": 9},
                            "role": "button",
                            "name": "Continue",
                            "children": []
                        }
                    ]
                }
            }
        });
        let text = render(&tree);

        assert_eq!(text, r#"- button "Continue" [ref=e1]"#);
    }

    #[test]
    fn diff_of_an_unchanged_page_is_empty() {
        let text = "- button \"Continue\" [ref=e1]\n";
        assert_eq!(unified_diff(text, text), "");
    }

    #[test]
    fn radio_candidates_use_the_native_html_input_name_as_a_group() {
        let snapshot = json!({
            "snapshot": {
                "root": {
                    "role": "rootWebArea",
                    "children": [{
                        "ref": {"ax_node_id": 21},
                        "role": "radioButton",
                        "name": "No trade-in",
                        "html_input_name": "tradeIn",
                        "checked": false,
                        "disabled": false,
                        "children": []
                    }]
                }
            }
        });
        let radios = matching_nodes(&snapshot, &json!({"kind": "role", "role": "radio"}));

        assert_eq!(radios[0].group.as_deref(), Some("input-name:tradeIn"));
        assert_eq!(
            matching_nodes(
                &snapshot,
                &json!({
                    "kind": "role",
                    "role": "radio",
                    "name": "No trade-in",
                    "exact": true,
                    "group": "input-name:tradeIn",
                }),
            )
            .len(),
            1
        );
        assert!(
            matching_nodes(
                &snapshot,
                &json!({
                    "kind": "role",
                    "role": "radio",
                    "name": "No trade-in",
                    "exact": true,
                    "group": "input-name:software",
                }),
            )
            .is_empty()
        );
    }
    #[derive(Default)]
    struct FakeBridge {
        calls: StdMutex<Vec<String>>,
        arguments: StdMutex<Vec<(String, Value)>>,
        fail_navigation_tab: StdMutex<Option<i32>>,
        transient_snapshot_failures: AtomicUsize,
        checkbox_checked: AtomicBool,
        probe_radio_checked: AtomicBool,
        delayed_enable_snapshots: AtomicUsize,
        delayed_radio_checked: AtomicBool,
        /// Number of `browser.eval` calls that answer `false` before the
        /// predicate flips true, so a wait has something real to poll.
        falsy_eval_polls: AtomicUsize,
        network_cursor: std::sync::atomic::AtomicI64,
    }

    #[async_trait]
    impl BrowserToolBridge for FakeBridge {
        async fn call_browser_tool(
            &self,
            _tool_call_id: &str,
            name: &str,
            arguments: Value,
            _cancel: CancellationToken,
        ) -> super::super::Result<ToolResultPayload> {
            self.calls.lock().unwrap().push(name.to_string());
            self.arguments
                .lock()
                .unwrap()
                .push((name.to_string(), arguments.clone()));
            if name == "browser.navigate"
                && positive_i32(arguments.get("tab_id"))
                    == *self.fail_navigation_tab.lock().unwrap()
            {
                return Ok(ToolResultPayload {
                    ok: false,
                    content: Value::Null,
                    error: Some("Tab was not found.".to_string()),
                    tainted: false,
                });
            }
            if name == "browser.snapshot"
                && self
                    .transient_snapshot_failures
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
            {
                return Ok(ToolResultPayload {
                    ok: false,
                    content: Value::Null,
                    error: Some("Frame-local accessibility snapshot was unavailable.".to_string()),
                    tainted: false,
                });
            }
            if name == "browser.click"
                && arguments.pointer("/ref/ax_node_id").and_then(Value::as_i64) == Some(5)
            {
                self.checkbox_checked.store(true, Ordering::Relaxed);
            }
            if name == "browser.click"
                && arguments.pointer("/ref/ax_node_id").and_then(Value::as_i64) == Some(9)
            {
                self.delayed_radio_checked.store(true, Ordering::Relaxed);
            }
            if name == "browser.mouse_click"
                && arguments.pointer("/point/x").and_then(Value::as_i64) == Some(610)
                && arguments.pointer("/point/y").and_then(Value::as_i64) == Some(420)
            {
                self.probe_radio_checked.store(true, Ordering::Relaxed);
            }
            let delayed_radio_disabled = name == "browser.snapshot"
                && self
                    .delayed_enable_snapshots
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok();
            if name == "browser.network_cursor" {
                return Ok(ToolResultPayload {
                    ok: true,
                    content: json!({ "cursor": self.network_cursor.load(Ordering::Relaxed) }),
                    error: None,
                    tainted: false,
                });
            }
            if name == "browser.network_log" {
                let after = arguments
                    .get("after_event_id")
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                // The fake publishes one record the first time it is polled past
                // the registration cursor, standing in for a response that lands
                // while the triggering click is still settling.
                let published = self.network_cursor.fetch_max(after + 1, Ordering::Relaxed);
                let _ = published;
                return Ok(ToolResultPayload {
                    ok: true,
                    content: json!({
                        "cursor": after + 1,
                        "records": [{
                            "event_id": after + 1,
                            "tab_id": 7,
                            "url": "https://example.com/api/orders?page=1",
                            "method": "GET",
                            "resource_type": "xhr",
                            "status": 200,
                            "ok": true,
                        }],
                    }),
                    error: None,
                    tainted: false,
                });
            }
            if name == "browser.eval" {
                let pending = self
                    .falsy_eval_polls
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok();
                return Ok(ToolResultPayload {
                    ok: true,
                    content: json!({ "json_result": if pending { "false" } else { "true" } }),
                    error: None,
                    tainted: false,
                });
            }
            let content = match name {
                "browser.list_tabs" => {
                    json!({ "tabs": [{ "tab_id": 7, "active": true, "title": "Example", "url": "https://example.com" }] })
                }
                "browser.open_tab" => json!({
                    "tab_id": 11,
                    "title": "Apple Store",
                    "url": arguments.get("url").cloned().unwrap_or(Value::Null),
                }),
                "browser.screenshot" => json!({
                    "tab_id": 7,
                    "mime_type": "image/png",
                    "image_base64": "aGVsbG8=",
                    "viewport_size": { "width": 1280, "height": 720 }
                }),
                "browser.probe_node" => json!({
                    "probe": {
                        "found": true,
                        "target_matched": true,
                        "visible": true,
                        "hit_test_stack": ["label.selector-card"],
                        "client_rect": { "x": 520, "y": 390, "width": 180, "height": 60 }
                    }
                }),
                "browser.list_credentials" => json!({
                    "credentials": [{
                        "handle": "credential-1",
                        "label": "jude@example.com",
                        "source": "password_manager",
                        "has_totp": false,
                        "has_passkey": false
                    }]
                }),
                "browser.snapshot" => json!({
                    "snapshot": {
                        "tab_id": 7,
                        "url": "https://example.com",
                        "title": "Example",
                        "generation": 1,
                        "root": {
                            "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 1 },
                            "role": "rootWebArea",
                            "name": "Example",
                            "children": [{
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 2 },
                                "role": "button",
                                "name": "Continue",
                                "clickable": true,
                                "bounds": { "x": 20, "y": 40, "width": 100, "height": 30 },
                                "disabled": false,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 10 },
                                "role": "link",
                                "name": "Buy - MacBook Air",
                                "url": "https://www.apple.com/ca/shop/buy-mac/macbook-air",
                                "disabled": false,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 3 },
                                "role": "textField",
                                "name": "Email",
                                "disabled": false,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 4 },
                                "role": "textField",
                                "name": "Password",
                                "disabled": false,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 5 },
                                "role": "checkBox",
                                "name": "Remember me",
                                "checked": self.checkbox_checked.load(Ordering::Relaxed),
                                "bounds": { "x": 20, "y": 90, "width": 180, "height": 32 },
                                "disabled": false,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 6 },
                                "role": "radioButton",
                                "name": "16GB",
                                "checked": "true",
                                "bounds": { "x": 20, "y": 130, "width": 180, "height": 32 },
                                "disabled": true,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 9 },
                                "role": "radioButton",
                                "name": "Delayed choice",
                                "checked": self.delayed_radio_checked.load(Ordering::Relaxed),
                                "bounds": { "x": 20, "y": 170, "width": 180, "height": 32 },
                                "disabled": delayed_radio_disabled,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 8 },
                                "role": "staticText",
                                "name": "13-inch",
                                "bounds": { "x": 1055, "y": 2800, "width": 80, "height": 24 },
                                "disabled": false,
                                "children": []
                            }, {
                                "ref": { "frame": { "tab_id": 7, "frame_token": "main", "snapshot_generation": 1 }, "ax_node_id": 7 },
                                "role": "radioButton",
                                "name": "13-inch",
                                "checked": self.probe_radio_checked.load(Ordering::Relaxed),
                                "bounds": { "x": 1040, "y": 2780, "width": 360, "height": 120 },
                                "disabled": false,
                                "children": []
                            }]
                        }
                    }
                }),
                _ => json!({ "result": { "ok": true } }),
            };
            Ok(ToolResultPayload {
                ok: true,
                content,
                error: None,
                tainted: false,
            })
        }
    }

    #[tokio::test]
    async fn playwright_locator_runs_through_native_bridge() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-1".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let result = tool
            .execute(
                "call-1",
                json!({ "tab_id": 7, "code": "return await page.getByRole('button', {name: 'Continue'}).click();" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(result.details["operations"].as_array().unwrap().len() >= 3);
        assert!(
            bridge
                .calls
                .lock()
                .unwrap()
                .contains(&"browser.click".to_string())
        );
    }

    #[tokio::test]
    async fn semantic_link_uses_its_resolved_destination() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-link-fallback".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        let result = tool
            .execute(
                "call-link-fallback",
                json!({
                    "tab_id": 7,
                    "code": "const link = page.getByRole('link', {name:'Buy - MacBook Air', exact:true}); const href = await link.getAttribute('href'); await link.click(); return href;"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("a semantic link should navigate by its AX URL");

        assert_eq!(
            result.details["result"],
            "https://www.apple.com/ca/shop/buy-mac/macbook-air"
        );
        assert!(bridge.arguments.lock().unwrap().iter().any(|(name, args)| {
            name == "browser.navigate"
                && args["url"] == "https://www.apple.com/ca/shop/buy-mac/macbook-air"
        }));
        assert!(
            !bridge
                .calls
                .lock()
                .unwrap()
                .contains(&"browser.click".to_string())
        );
    }

    #[tokio::test]
    async fn repl_state_persists_for_a_session() {
        let bridge = Arc::new(FakeBridge::default());
        let pool = Arc::new(BrowserRuntimePool::default());
        let tool = BrowserCodeTool::new(
            "session-1".to_string(),
            bridge,
            Arc::new(BrowserPerceptionState::default()),
            pool,
        );
        tool.execute(
            "call-1",
            json!({ "code": "state.counter = 41; return state.counter;" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let result = tool
            .execute(
                "call-2",
                json!({ "code": "state.counter += 1; return state.counter;" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.details["result"], 42);
    }

    #[tokio::test]
    async fn zero_tab_id_opens_an_agent_owned_page_instead_of_navigating_tab_zero() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-zero-tab".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        let result = tool
            .execute(
                "call-zero-tab",
                json!({
                    "tab_id": 0,
                    "code": "return await page.goto('https://www.apple.com/ca/store');"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("tab zero should self-heal by opening an agent tab");

        assert_eq!(result.details["result"]["tab_id"], 11);
        let arguments = bridge.arguments.lock().unwrap();
        let (_, open_args) = arguments
            .iter()
            .find(|(name, _)| name == "browser.open_tab")
            .expect("page.goto should open an agent-owned tab");
        assert_eq!(open_args["agent_owned"], true);
        assert_eq!(open_args["url"], "https://www.apple.com/ca/store");
        assert!(!arguments.iter().any(|(name, args)| {
            name == "browser.navigate" && args.get("tab_id") == Some(&json!(0))
        }));
    }

    #[tokio::test]
    async fn stale_tab_navigation_recovers_into_a_new_agent_owned_page() {
        let bridge = Arc::new(FakeBridge::default());
        *bridge.fail_navigation_tab.lock().unwrap() = Some(77);
        let tool = BrowserCodeTool::new(
            "session-stale-tab".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        let result = tool
            .execute(
                "call-stale-tab",
                json!({
                    "tab_id": 77,
                    "code": "return await page.goto('https://www.apple.com/ca/store');"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("stale tab navigation should recover");

        assert_eq!(result.details["result"]["tab_id"], 11);
        let calls = bridge.calls.lock().unwrap();
        assert!(calls.contains(&"browser.navigate".to_string()));
        assert!(calls.contains(&"browser.open_tab".to_string()));
        drop(calls);

        bridge.arguments.lock().unwrap().clear();
        tool.execute(
            "call-after-stale-tab",
            json!({
                // The UI can still supply the attachment id from the beginning of the
                // turn. The persistent Page must retain the recovered tab instead.
                "tab_id": 77,
                "code": "return await page.title();"
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("the recovered page should persist across executions");
        let arguments = bridge.arguments.lock().unwrap();
        let (_, info_args) = arguments
            .iter()
            .find(|(name, _)| name == "browser.snapshot")
            .expect("page.title should inspect the recovered tab");
        assert_eq!(info_args["tab_id"], 11);
    }

    #[tokio::test]
    async fn repeated_browser_failures_return_a_user_facing_boundary() {
        let tool = BrowserCodeTool::new(
            "session-failure-boundary".to_string(),
            Arc::new(FakeBridge::default()),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        for call in 1..MAX_CONSECUTIVE_BROWSER_EXEC_FAILURES {
            let error = tool
                .execute(
                    &format!("call-failure-{call}"),
                    json!({ "code": "throw new Error('synthetic browser failure');" }),
                    CancellationToken::new(),
                    None,
                )
                .await
                .expect_err("early failures should remain visible to the model");
            assert!(error.to_string().contains("synthetic browser failure"));
        }

        let stopped = tool
            .execute(
                "call-failure-final",
                json!({ "code": "throw new Error('synthetic browser failure');" }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("the failure boundary should return a recoverable result");
        assert_eq!(stopped.terminate, None);
        assert_eq!(stopped.details["status"], "browser_execution_stopped");
    }

    #[tokio::test]
    async fn screenshot_bytes_are_forwarded_as_multimodal_content() {
        let tool = BrowserCodeTool::new(
            "session-1".to_string(),
            Arc::new(FakeBridge::default()),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let result = tool
            .execute(
                "call-1",
                json!({ "tab_id": 7, "code": "return await page.screenshot();" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.details["screenshots_attached"], 1);
        assert!(result.content.iter().any(|block| {
            matches!(block, pie_ai::UserContentBlock::Image(image) if image.data == "aGVsbG8=" && image.mime_type == "image/png")
        }));
    }

    #[tokio::test]
    async fn playwright_compatibility_methods_lower_to_native_actions() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-1".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        tool.execute(
            "call-1",
            json!({
                "tab_id": 7,
                "code": "await page.getByRole('button', {name: 'Continue'}).hover(); await page.getByRole('checkbox', {name: 'Remember me'}).check(); return await page.getByLabel('Email').isEnabled();"
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

        let calls = bridge.calls.lock().unwrap();
        assert!(calls.contains(&"browser.mouse_move".to_string()));
        assert!(calls.contains(&"browser.click".to_string()));
    }

    #[tokio::test]
    async fn custom_radio_fallback_clicks_the_live_client_rect_and_verifies_state() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-custom-radio".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        let result = tool
            .execute(
                "call-custom-radio",
                json!({
                    "tab_id": 7,
                    "code": "const size = page.getByRole('radio', {name:'13-inch'}); await size.click(); return await size.isChecked();"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("custom radio should use its live viewport rect");

        assert_eq!(result.details["result"], true);
        let arguments = bridge.arguments.lock().unwrap();
        assert!(
            arguments
                .iter()
                .any(|(name, _)| name == "browser.scroll_into_view")
        );
        assert!(arguments.iter().any(|(name, args)| {
            name == "browser.mouse_click" && args["point"] == json!({ "x": 610, "y": 420 })
        }));
        assert!(!arguments.iter().any(|(name, args)| {
            name == "browser.mouse_click" && args["point"] == json!({ "x": 1220, "y": 2840 })
        }));
    }

    #[tokio::test]
    async fn text_click_promotes_a_matching_radio_over_a_static_text_leaf() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-text-radio".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        let result = tool
            .execute(
                "call-text-radio",
                json!({
                    "tab_id": 7,
                    "code": "const size = page.getByText('13-inch').first(); await size.click(); return await page.getByRole('radio', {name:'13-inch'}).isChecked();"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("text click should activate and verify the matching radio");

        assert_eq!(result.details["result"], true);
        assert!(bridge.arguments.lock().unwrap().iter().any(|(name, args)| {
            name == "browser.mouse_click" && args["point"] == json!({ "x": 610, "y": 420 })
        }));
    }

    #[tokio::test]
    async fn button_shaped_choice_falls_back_to_a_named_radio() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-button-radio".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        let result = tool
            .execute(
                "call-button-radio",
                json!({
                    "tab_id": 7,
                    "code": "await page.getByRole('button', {name:'13-inch'}).click(); return await page.getByRole('radio', {name:'13-inch'}).isChecked();"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("a button-shaped choice should activate its named radio");

        assert_eq!(result.details["result"], true);
    }

    #[tokio::test]
    async fn action_waits_for_a_dependent_radio_to_become_enabled() {
        let bridge = Arc::new(FakeBridge::default());
        bridge.delayed_enable_snapshots.store(2, Ordering::Relaxed);
        let tool = BrowserCodeTool::new(
            "session-dependent-radio".to_string(),
            bridge,
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        let result = tool
            .execute(
                "call-dependent-radio",
                json!({
                    "tab_id": 7,
                    "code": "const choice=page.getByRole('radio',{name:'Delayed choice'}); await choice.check(); return await choice.isChecked();"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("a dependent radio should auto-wait until enabled");

        assert_eq!(result.details["result"], true);
    }

    #[tokio::test]
    async fn common_playwright_read_methods_are_compatible() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-playwright-reads".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let result = tool
            .execute(
                "call-playwright-reads",
                json!({
                    "code": "const p = await context.newPage('https://example.com'); const snapshot = await p.snapshot(); const preview = snapshot.slice(0, 20); const matches = snapshot.match(/Example/g); await new Promise(resolve => setTimeout(resolve, 1)); const href = await p.getByRole('link', {name:'Missing'}).getAttribute('href').catch(() => null); const buttons = p.getByRole('button'); const exactButtonCount = await p.getByRole('button', {name:/^Continue$/}).count(); const filteredRadioCount = await p.getByRole('radio').filter({hasText:/16GB|13-inch/}).count(); const allButtons = await buttons.all(); const buttonTexts = await buttons.allInnerTexts(); const radios = await p.locator('input[type=radio]').evaluateAll(es => es.map(e => ({checked:e.checked, label:[...document.querySelectorAll(`label[for=\"${e.id}\"]`)].map(x=>x.innerText)}))); const lastChecked = await p.getByRole('radio', {name:'16GB'}).last().isChecked(); const checkedRadios = await p.getByRole('radio', {checked:true}).allTextContents(); const radioLabel = await p.getByRole('radio', {name:'16GB'}).getAttribute('aria-label'); await p.locator('body').innerText(); return {url:p.url(), previewType:typeof preview, matches:Array.isArray(matches), href, exactButtonCount, filteredRadioCount, allButtonCount:allButtons.length, buttonTexts, radios, lastChecked, checkedRadios, radioLabel};"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.details["result"]["url"], "https://example.com");
        assert_eq!(result.details["result"]["previewType"], "string");
        assert_eq!(result.details["result"]["matches"], true);
        assert_eq!(result.details["result"]["exactButtonCount"], 1);
        assert_eq!(result.details["result"]["filteredRadioCount"], 2);
        assert_eq!(result.details["result"]["allButtonCount"], 1);
        assert_eq!(result.details["result"]["buttonTexts"][0], "Continue");
        assert_eq!(result.details["result"]["radios"][0]["checked"], true);
        assert_eq!(result.details["result"]["radios"][0]["label"][0], "16GB");
        assert_eq!(result.details["result"]["lastChecked"], true);
        assert_eq!(result.details["result"]["checkedRadios"][0], "16GB");
        assert_eq!(result.details["result"]["radioLabel"], "16GB");
        let calls = bridge.calls.lock().unwrap();
        assert!(!calls.contains(&"browser.eval".to_string()));
    }

    #[tokio::test]
    async fn a_new_page_becomes_the_persistent_default_page() {
        let tool = BrowserCodeTool::new(
            "session-default-page".to_string(),
            Arc::new(FakeBridge::default()),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let opened = tool
            .execute(
                "call-open-default",
                json!({"code": "await context.newPage('https://example.com'); return page._tabId;"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let resumed = tool
            .execute(
                "call-resume-default",
                json!({"code": "return page._tabId;"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(opened.details["result"], 11);
        assert_eq!(resumed.details["result"], 11);
    }

    #[test]
    fn retina_bounds_are_normalized_to_the_reported_viewport() {
        let adjusted = adjusted_point_for_viewport(
            &json!({ "x": 1747.0, "y": 1297.0 }),
            "Mouse point is outside the 1099x912 visible viewport.",
        )
        .expect("retina point should be recoverable");
        assert_eq!(adjusted["x"], 873.5);
        assert_eq!(adjusted["y"], 648.5);
    }

    #[test]
    fn empty_probe_hit_stack_marks_an_offscreen_control_for_wheel_scrolling() {
        assert!(probe_requires_wheel_scroll(&json!({
            "probe": {
                "client_rect": { "x": 712, "y": 1115, "width": 42, "height": 42 },
                "hit_test_stack": []
            }
        })));
        assert!(!probe_requires_wheel_scroll(&json!({
            "probe": {
                "client_rect": { "x": 712, "y": 515, "width": 42, "height": 42 },
                "hit_test_stack": ["label.color"]
            }
        })));
    }

    #[tokio::test]
    async fn browser_exec_compacts_large_results_and_deduplicates_images() {
        let tool = BrowserCodeTool::new(
            "session-compact-result".to_string(),
            Arc::new(FakeBridge::default()),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let result = tool
            .execute(
                "call-compact-result",
                json!({
                    "tab_id": 7,
                    "code": "console.log('y'.repeat(50000)); await page.screenshot(); await page.screenshot(); return 'x'.repeat(100000);"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        let text_bytes = result
            .content
            .iter()
            .filter_map(|block| match block {
                pie_ai::UserContentBlock::Text(text) => Some(text.text.len()),
                _ => None,
            })
            .sum::<usize>();
        let image_count = result
            .content
            .iter()
            .filter(|block| matches!(block, pie_ai::UserContentBlock::Image(_)))
            .count();
        assert!(text_bytes < 40 * 1024, "browser result should be compact");
        assert!(result.details.to_string().len() < 40 * 1024);
        assert_eq!(
            image_count, 1,
            "identical screenshots should be deduplicated"
        );
    }

    #[tokio::test]
    async fn playwright_radio_alias_and_disabled_scroll_are_compatible() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-radio-compatibility".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let result = tool
            .execute(
                "call-radio-compatibility",
                json!({
                    "tab_id": 7,
                    "code": "const radio=page.getByRole('radio',{name:'16GB'}); const count=await radio.count(); const checked=await radio.getAttribute('aria-checked'); await radio.scrollIntoViewIfNeeded(); return {count,checked};"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("Playwright radio locators should map to AX radioButton nodes");

        assert_eq!(result.details["result"]["count"], 1);
        assert_eq!(result.details["result"]["checked"], "true");
        assert!(
            bridge
                .calls
                .lock()
                .unwrap()
                .contains(&"browser.scroll_into_view".to_string())
        );
    }

    #[tokio::test]
    async fn wheel_scroll_targets_the_last_mouse_position() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-wheel-target".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        tool.execute(
            "call-wheel-target",
            json!({
                "tab_id": 7,
                "code": "await page.mouse.move(940, 520); return await page.mouse.wheel(0, 700);"
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

        let arguments = bridge.arguments.lock().unwrap();
        let (_, scroll_args) = arguments
            .iter()
            .find(|(name, _)| name == "browser.scroll")
            .expect("wheel should lower to native scrolling");
        assert_eq!(scroll_args["point"], json!({ "x": 940, "y": 520 }));
        assert_eq!(scroll_args["dy"], 700);
    }

    #[tokio::test]
    async fn navigation_retries_transient_frame_snapshot_races() {
        let bridge = Arc::new(FakeBridge::default());
        bridge
            .transient_snapshot_failures
            .store(2, Ordering::Relaxed);
        let tool = BrowserCodeTool::new(
            "session-snapshot-race".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );

        tool.execute(
            "call-snapshot-race",
            json!({
                "tab_id": 7,
                "code": "await page.goto('https://example.com/next'); return await page.title();"
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("transient frame replacement should settle and recover");

        let snapshot_calls = bridge
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|name| name.as_str() == "browser.snapshot")
            .count();
        assert!(snapshot_calls >= 3);
    }

    #[tokio::test]
    async fn credential_fill_resolves_semantic_locators_without_exposing_secrets() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-1".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        tool.execute(
            "call-1",
            json!({
                "tab_id": 7,
                "code": "const credential = (await stead.credentials.list())[0]; return await stead.credentials.fill(credential, page.getByLabel('Email'), page.getByLabel('Password'));"
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

        let arguments = bridge.arguments.lock().unwrap();
        let (_, fill) = arguments
            .iter()
            .find(|(name, _)| name == "browser.fill_credential")
            .expect("credential fill should reach the native broker");
        assert_eq!(fill["credential"]["handle"], "credential-1");
        assert_eq!(fill["username_field"]["ax_node_id"], 3);
        assert_eq!(fill["password_field"]["ax_node_id"], 4);
        assert!(!fill.to_string().contains("password_value"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_interrupts_runaway_javascript() {
        let tool = BrowserCodeTool::new(
            "session-1".to_string(),
            Arc::new(FakeBridge::default()),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            tool.execute("call-1", json!({ "code": "while (true) {}" }), cancel, None),
        )
        .await
        .expect("QuickJS should be interruptible")
        .expect_err("cancelled JavaScript should fail");
        assert!(result.to_string().contains("cancelled"));
    }

    #[test]
    fn url_globs_follow_playwright_separator_rules() {
        let matches = |glob: &str, url: &str| url_pattern_matches(&json!({ "glob": glob }), url);

        assert!(matches("https://example.com/a", "https://example.com/a"));
        // A single star stops at a path separator; a double star crosses it.
        assert!(matches(
            "https://example.com/*",
            "https://example.com/orders"
        ));
        assert!(!matches(
            "https://example.com/*",
            "https://example.com/orders/17"
        ));
        assert!(matches(
            "https://example.com/**",
            "https://example.com/orders/17"
        ));
        assert!(matches(
            "**/checkout?step=*",
            "https://shop.test/checkout?step=2"
        ));
        // Regex metacharacters in a glob stay literal.
        assert!(!matches(
            "https://example.com/a.b",
            "https://example.com/axb"
        ));
    }

    #[test]
    fn url_regex_patterns_honor_javascript_flags() {
        assert!(url_pattern_matches(
            &json!({ "regex": "EXAMPLE", "flags": "i" }),
            "https://example.com/a"
        ));
        assert!(!url_pattern_matches(
            &json!({ "regex": "EXAMPLE", "flags": "" }),
            "https://example.com/a"
        ));
    }

    #[test]
    fn wait_states_collapse_onto_observable_presence() {
        assert_eq!(wait_state_expects_presence("attached"), Ok(true));
        assert_eq!(wait_state_expects_presence("visible"), Ok(true));
        assert_eq!(wait_state_expects_presence("detached"), Ok(false));
        assert_eq!(wait_state_expects_presence("hidden"), Ok(false));
        assert!(wait_state_expects_presence("settled").is_err());
    }

    #[test]
    fn wait_timeout_clamps_and_treats_zero_as_unbounded() {
        assert_eq!(
            wait_timeout(
                &json!({ "options": { "timeout": 250 } }),
                DEFAULT_WAIT_TIMEOUT
            ),
            Duration::from_millis(250)
        );
        assert_eq!(
            wait_timeout(
                &json!({ "options": { "timeout": 0 } }),
                DEFAULT_WAIT_TIMEOUT
            ),
            MAX_WAIT_TIMEOUT
        );
        assert_eq!(
            wait_timeout(&json!({ "options": {} }), DEFAULT_WAIT_TIMEOUT),
            DEFAULT_WAIT_TIMEOUT
        );
        assert_eq!(
            wait_timeout(
                &json!({ "options": { "timeout": 9_999_999u64 } }),
                DEFAULT_WAIT_TIMEOUT
            ),
            MAX_WAIT_TIMEOUT
        );
    }

    async fn run_exec(session: &str, bridge: Arc<FakeBridge>, code: &str) -> AgentToolResult {
        let tool = BrowserCodeTool::new(
            session.to_string(),
            bridge,
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        tool.execute(
            "call-wait",
            json!({ "tab_id": 7, "code": code }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("browser_exec should succeed")
    }

    #[tokio::test]
    async fn wait_for_url_resolves_an_already_satisfied_pattern() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-wait-url",
            bridge,
            "return await page.waitForURL('https://example.com');",
        )
        .await;

        assert_eq!(result.details["result"], "https://example.com");
    }

    #[tokio::test]
    async fn wait_for_url_timeout_reports_where_the_page_actually_is() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-wait-url-timeout".to_string(),
            bridge,
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let error = tool
            .execute(
                "call-wait-timeout",
                json!({
                    "tab_id": 7,
                    "code": "return await page.waitForURL('https://checkout.example/**');",
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("a URL that never arrives should fail loudly");

        let message = error.to_string();
        assert!(message.contains("timed out"), "{message}");
        // The failure has to name the observed URL, otherwise the model's only
        // recovery is to guess and re-run the same wait.
        assert!(message.contains("https://example.com"), "{message}");
    }

    #[tokio::test]
    async fn wait_for_url_predicate_receives_a_parsed_url() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-wait-url-predicate",
            bridge,
            "return await page.waitForURL((url) => url.hostname === 'example.com' && url.protocol === 'https:');",
        )
        .await;

        assert_eq!(result.details["result"], "https://example.com");
    }

    #[tokio::test]
    async fn wait_for_selector_returns_a_locator_and_resolves_absence() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-wait-selector",
            bridge,
            "const found = await page.waitForSelector('button'); \
             const missing = await page.waitForSelector('dialog', {state: 'detached'}); \
             return {found: found !== null, missing};",
        )
        .await;

        assert_eq!(result.details["result"]["found"], true);
        assert_eq!(result.details["result"]["missing"], Value::Null);
    }

    #[tokio::test]
    async fn locator_wait_for_polls_instead_of_reporting_a_hardcoded_state() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-locator-wait",
            bridge,
            "return await page.getByRole('button', {name: 'Continue'}).waitFor();",
        )
        .await;

        assert_eq!(result.details["result"]["state"], "visible");
        assert_eq!(result.details["result"]["matches"], 1);
    }

    #[tokio::test]
    async fn locator_wait_for_times_out_on_an_element_that_never_appears() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-locator-wait-timeout".to_string(),
            bridge,
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let error = tool
            .execute(
                "call-locator-wait-timeout",
                json!({
                    "tab_id": 7,
                    "code": "return await page.getByRole('button', {name: 'Place order'}).waitFor();",
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("waitFor must not report success for a missing element");

        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn wait_for_function_polls_the_page_until_the_predicate_is_truthy() {
        let bridge = Arc::new(FakeBridge::default());
        bridge.falsy_eval_polls.store(2, Ordering::Relaxed);
        let result = run_exec(
            "session-wait-function",
            bridge.clone(),
            "return await page.waitForFunction(() => window.__ready === true);",
        )
        .await;

        assert_eq!(result.details["result"], true);
        let evaluations = bridge
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| *call == "browser.eval")
            .count();
        // Two falsy answers then a truthy one: the wait polled natively instead
        // of handing the retry back to the model.
        assert_eq!(evaluations, 3);
        // The frame is resolved once and reused across polls, so a 30s wait
        // costs one snapshot rather than one per poll.
        let snapshots = bridge
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| *call == "browser.snapshot")
            .count();
        assert_eq!(snapshots, 1);
    }

    #[tokio::test]
    async fn wait_for_load_state_maps_states_onto_ready_state() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-wait-load-state",
            bridge.clone(),
            "return await page.waitForLoadState('domcontentloaded');",
        )
        .await;

        assert_eq!(result.details["result"]["state"], "domcontentloaded");
        assert!(bridge.arguments.lock().unwrap().iter().any(|(name, args)| {
            name == "browser.eval"
                && args["js"]
                    .as_str()
                    .is_some_and(|js| js.contains("document.readyState !== 'loading'"))
        }));
    }

    #[test]
    fn candidate_summary_names_matches_and_caps_the_list() {
        let node = |role: &str, name: &str| CandidateNode {
            reference: Value::Null,
            role: role.to_string(),
            name: name.to_string(),
            href: None,
            bounds: None,
            value: None,
            checked: None,
            disabled: false,
            group: None,
        };
        assert_eq!(
            candidate_summary(&[node("radio", "16GB"), node("radio", "24GB")]),
            "radio \"16GB\", radio \"24GB\""
        );
        let many: Vec<CandidateNode> = (0..6)
            .map(|index| node("link", &index.to_string()))
            .collect();
        assert!(candidate_summary(&many).ends_with("and 2 more"));
    }

    #[tokio::test]
    async fn an_ambiguous_locator_fails_immediately_instead_of_guessing() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-strict".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let error = tool
            .execute(
                "call-strict",
                json!({ "tab_id": 7, "code": "return await page.getByRole('radio').click();" }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("a bare locator matching several elements must not silently pick one");

        let message = error.to_string();
        assert!(message.contains("resolved to 3 elements"), "{message}");
        // The model can only narrow if it is told what was matched.
        assert!(message.contains("16GB"), "{message}");
        assert!(message.contains(".first()"), "{message}");
        // Ambiguity is permanent: waiting cannot fix it, so nothing re-polls.
        let snapshots = bridge
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| *call == "browser.snapshot")
            .count();
        assert_eq!(snapshots, 1);
    }

    #[tokio::test]
    async fn first_and_nth_opt_out_of_strict_mode() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-strict-optout",
            bridge,
            "const all = await page.getByRole('radio').count(); \
             const named = await page.getByRole('radio').first().getAttribute('aria-label'); \
             return {all, named};",
        )
        .await;

        assert_eq!(result.details["result"]["all"], 3);
        assert!(result.details["result"]["named"].is_string());
    }

    #[tokio::test]
    async fn a_locator_that_matches_nothing_polls_then_fails_with_recovery_guidance() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-missing".to_string(),
            bridge.clone(),
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let error = tool
            .execute(
                "call-missing",
                json!({
                    "tab_id": 7,
                    "code": "return await page.getByRole('button', {name: 'Place order'}).click();",
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("an element that never appears should fail");

        let message = error.to_string();
        assert!(
            message.contains("matched no elements after waiting"),
            "{message}"
        );
        assert!(message.contains("waitForLoadState"), "{message}");
        // Absence is transient, so it re-resolved before giving up.
        let snapshots = bridge
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| *call == "browser.snapshot")
            .count();
        assert!(snapshots > 1, "expected polling, saw {snapshots} snapshots");
    }

    #[tokio::test]
    async fn wait_for_response_registers_before_the_action_that_triggers_it() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-wait-response",
            bridge.clone(),
            "const wait = page.waitForResponse('**/api/orders*'); \
             await page.getByRole('button', {name: 'Continue'}).click(); \
             return await wait;",
        )
        .await;

        assert_eq!(result.details["result"]["status"], 200);
        assert_eq!(result.details["result"]["ok"], true);

        // The whole point of the Playwright shape: the cursor is captured before
        // the click runs, so a response that lands during the click cannot be
        // missed. QuickJS serializes native calls, so call order proves it.
        let calls = bridge.calls.lock().unwrap();
        let registered = calls
            .iter()
            .position(|call| call == "browser.network_cursor")
            .expect("waitForResponse should register a cursor");
        let clicked = calls
            .iter()
            .position(|call| call == "browser.click")
            .expect("the click should run");
        assert!(
            registered < clicked,
            "registration must precede the trigger: {calls:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_response_accepts_a_predicate_and_filters_in_javascript() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-wait-response-predicate",
            bridge,
            "const wait = page.waitForResponse((response) => response.url.includes('/api/orders') && response.ok); \
             await page.getByRole('button', {name: 'Continue'}).click(); \
             const response = await wait; \
             return {url: response.url, method: response.method};",
        )
        .await;

        assert_eq!(
            result.details["result"]["url"],
            "https://example.com/api/orders?page=1"
        );
        assert_eq!(result.details["result"]["method"], "GET");
    }

    #[tokio::test]
    async fn set_input_files_arms_the_chooser_before_activating_the_input() {
        let bridge = Arc::new(FakeBridge::default());
        let _ = run_exec(
            "session-upload",
            bridge.clone(),
            "return await page.getByRole('button', {name: 'Continue'})\
               .setInputFiles('/Users/jude/resume.pdf');",
        )
        .await;

        let arguments = bridge.arguments.lock().unwrap();
        let armed = arguments
            .iter()
            .position(|(name, _)| name == "browser.set_input_files")
            .expect("setInputFiles should arm the chooser");
        let activated = arguments
            .iter()
            .position(|(name, _)| name == "browser.click")
            .expect("setInputFiles should activate the input");
        // Arming after the click would leave the chooser waiting on a user.
        assert!(armed < activated, "arming must precede activation");
        assert_eq!(
            arguments[armed].1["paths"],
            json!(["/Users/jude/resume.pdf"])
        );
    }

    #[tokio::test]
    async fn interactive_snapshot_mints_handles_that_survive_a_rerender() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-refs",
            bridge.clone(),
            "const snap = await page.snapshot({interactive: true}); \
             const button = snap.elements.find((element) => element.name === 'Continue'); \
             await page.locator('@' + button.ref).click(); \
             return {ref: button.ref, total: snap.elements.length, \
                     hasHeadings: snap.elements.some((e) => e.role === 'heading')};",
        )
        .await;

        let value = &result.details["result"];
        assert!(value["ref"].as_str().unwrap().starts_with('e'));
        // Only actionable elements: the staticText and rootWebArea are excluded.
        assert!(value["total"].as_u64().unwrap() >= 5);
        assert_eq!(value["hasHeadings"], false);
        // The handle resolved to a real native click, not a lookup failure.
        assert!(
            bridge
                .calls
                .lock()
                .unwrap()
                .contains(&"browser.click".to_string())
        );
    }

    #[tokio::test]
    async fn an_unknown_handle_says_how_to_get_a_fresh_one() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-stale-ref".to_string(),
            bridge,
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let error = tool
            .execute(
                "call-stale-ref",
                json!({ "tab_id": 7, "code": "return await page.locator('@e99').click();" }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("a handle that was never minted should fail");

        let message = error.to_string();
        assert!(message.contains("unknown element handle e99"), "{message}");
        assert!(message.contains("interactive: true"), "{message}");
    }

    #[tokio::test]
    async fn check_activates_a_custom_radio_with_no_accessible_checked_state() {
        let bridge = Arc::new(FakeBridge::default());
        // "Continue" is a plain button: it exposes no checked state, standing in
        // for a div-built radio whose AX node omits one.
        let result = run_exec(
            "session-custom-check",
            bridge.clone(),
            "return await page.getByRole('button', {name: 'Continue'}).check();",
        )
        .await;

        assert!(result.details["result"].is_object());
        // It must activate the control, not reject the operation outright.
        assert!(
            bridge
                .calls
                .lock()
                .unwrap()
                .contains(&"browser.click".to_string()),
            "check() on a stateless control should still activate it"
        );
    }

    #[test]
    fn observation_diff_reports_the_change_not_the_page() {
        let element = |role: &str, name: &str, disabled: bool, checked: Value| json!({ "role": role, "name": name, "disabled": disabled, "checked": checked, "value": null });
        let previous = json!({
            "url": "https://example.com/config",
            "elements": [
                element("radioButton", "16GB", false, json!(true)),
                element("radioButton", "24GB", false, json!(false)),
                element("button", "Continue", true, Value::Null),
            ],
        });
        let next = json!({
            "url": "https://example.com/config",
            "title": "Configure",
            "elements": [
                element("radioButton", "16GB", false, json!(true)),
                element("radioButton", "24GB", false, json!(false)),
                // Continue became enabled, and a new group mounted.
                element("button", "Continue", false, Value::Null),
                element("radioButton", "512GB", false, json!(false)),
            ],
        });

        let diff = diff_browser_observation(&previous, &next).expect("same document diffs");
        assert_eq!(diff["added"].as_array().unwrap().len(), 1);
        assert_eq!(diff["added"][0]["name"], "512GB");
        assert_eq!(diff["updated"].as_array().unwrap().len(), 1);
        assert_eq!(diff["updated"][0]["name"], "Continue");
        assert_eq!(diff["updated"][0]["from"]["disabled"], true);
        assert_eq!(diff["updated"][0]["to"]["disabled"], false);
        assert!(diff["removed"].as_array().unwrap().is_empty());
        // The two untouched radios cost a count, not two more objects.
        assert_eq!(diff["unchanged_count"], 2);
        // And the diff is materially smaller than resending the page.
        assert!(diff.to_string().len() < next.to_string().len());
    }

    #[test]
    fn observation_diff_falls_back_across_a_navigation() {
        let previous = json!({ "url": "https://example.com/a", "elements": [] });
        let next = json!({ "url": "https://example.com/b", "elements": [] });
        // A diff across documents is noise; the caller sends the full observation.
        assert!(diff_browser_observation(&previous, &next).is_none());
    }

    #[test]
    fn observation_diff_collapses_a_no_op_action() {
        let page = json!({
            "url": "https://example.com",
            "elements": [json!({ "role": "button", "name": "Continue", "disabled": false, "checked": null, "value": null })],
        });
        let diff = diff_browser_observation(&page, &page).expect("same document");
        assert_eq!(diff["unchanged"], true);
    }

    #[tokio::test]
    async fn help_answers_signature_questions_without_a_model_round_trip() {
        let bridge = Arc::new(FakeBridge::default());
        let result = run_exec(
            "session-help",
            bridge,
            "return {\
               topics: help(),\
               exact: help('page.waitForURL'),\
               grouped: help('locator').split('\\n').length,\
               typo: help('waitForRespons'),\
               unknown: help('page.teleport'),\
             };",
        )
        .await;

        let value = &result.details["result"];
        assert!(value["topics"].as_str().unwrap().contains("page"));
        assert!(
            value["exact"]
                .as_str()
                .unwrap()
                .contains("page.waitForURL(urlOrRegExpOrPredicate")
        );
        assert!(value["grouped"].as_u64().unwrap() > 20);
        // A near miss should point at the real name rather than just failing.
        assert!(
            value["typo"]
                .as_str()
                .unwrap()
                .contains("page.waitForResponse")
        );
        assert!(value["unknown"].as_str().unwrap().contains("help()"));
    }

    #[tokio::test]
    async fn drag_to_lowers_onto_native_mouse_drag_between_element_centers() {
        let bridge = Arc::new(FakeBridge::default());
        let _ = run_exec(
            "session-drag",
            bridge.clone(),
            "return await page.getByRole('button', {name: 'Continue'})\
               .dragTo(page.getByRole('checkbox', {name: 'Remember me'}));",
        )
        .await;

        let arguments = bridge.arguments.lock().unwrap();
        let (_, drag) = arguments
            .iter()
            .find(|(name, _)| name == "browser.mouse_drag")
            .expect("dragTo should lower to a native drag");
        assert_eq!(drag["from"], json!({ "x": 70, "y": 55 }));
        assert_eq!(drag["to"], json!({ "x": 110, "y": 106 }));
    }

    #[tokio::test]
    async fn set_default_timeout_applies_to_waits_that_do_not_override_it() {
        async fn snapshots_for(session: &str, prelude: &str) -> usize {
            let bridge = Arc::new(FakeBridge::default());
            let tool = BrowserCodeTool::new(
                session.to_string(),
                bridge.clone(),
                Arc::new(BrowserPerceptionState::default()),
                Arc::new(BrowserRuntimePool::default()),
            );
            tool.execute(
                "call-default-timeout",
                json!({
                    "tab_id": 7,
                    "code": format!(
                        "{prelude} try {{ await page.waitForURL('https://never.example/'); }} \
                         catch (_) {{}} return true;"
                    ),
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("the script swallows the timeout itself");
            let calls = bridge.calls.lock().unwrap();
            calls
                .iter()
                .filter(|call| *call == "browser.snapshot")
                .count()
        }

        let baseline = snapshots_for("session-timeout-baseline", "").await;
        let shortened = snapshots_for("session-timeout-set", "page.setDefaultTimeout(120);").await;

        // A shorter page default has to mean fewer polls before giving up;
        // asserting a ratio rather than an exact count keeps this honest
        // without pinning the poll arithmetic.
        assert!(
            shortened < baseline,
            "setDefaultTimeout did not shorten the wait: {shortened} vs {baseline}"
        );
    }

    #[tokio::test]
    async fn wait_for_load_state_rejects_an_unknown_state() {
        let bridge = Arc::new(FakeBridge::default());
        let tool = BrowserCodeTool::new(
            "session-wait-load-state-invalid".to_string(),
            bridge,
            Arc::new(BrowserPerceptionState::default()),
            Arc::new(BrowserRuntimePool::default()),
        );
        let error = tool
            .execute(
                "call-load-state-invalid",
                json!({ "tab_id": 7, "code": "return await page.waitForLoadState('settled');" }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect_err("an unsupported load state should say so");

        assert!(
            error.to_string().contains("unsupported load state"),
            "{error}"
        );
    }
}
