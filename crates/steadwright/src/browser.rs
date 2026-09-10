use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use crate::input::InputState;
use base64::Engine as _;
use serde_json::{Map, Value, json};
use steadwright_cdp::{Connection, Event, Session, Transport, WebSocketTransport};
use tokio::sync::Notify;

use crate::{
    CallArg, ConsoleMessage, DEFAULT_TIMEOUT, Deadline, DialogType, Error, GotoOptions,
    IntoTimeout, JsValue, Keyboard, LoadState, Mouse, PageError, PageEvent, Request, Response,
    Result, ScreenshotFormat, ScreenshotOptions, ScreenshotScale, Touchscreen, UrlMatcher,
    ViewportSize, WaitForUrlOptions, World,
};

const UTILITY_WORLD: &str = "__steadwright";
const PLAYWRIGHT_INJECTED: &str = include_str!("../vendor/playwright/injectedScriptSource.js");
const PLAYWRIGHT_UTILITY: &str = include_str!("../vendor/playwright/utilityScriptSource.js");
const COLLECTION_MARKER: &str = "__steadwright_collection_63e9c4d8b2744a55a62fe723110cfd91__";
const EVALUATION_WRAPPER: &str = r#"(function(__swExpression, __swReturnByValue, ...__swArgs) {
  const __swTag = "__steadwright_collection_63e9c4d8b2744a55a62fe723110cfd91__";
  const __swOwn = (value, key) => Object.prototype.hasOwnProperty.call(value, key);
  const __swRevive = (value, seen) => {
    if (!value || typeof value !== "object") return value;
    if (seen.has(value)) return seen.get(value);
    const keys = Object.keys(value);
    if (keys.length === 2 && __swOwn(value, __swTag) && __swOwn(value, "value")) {
      if (value[__swTag] === "map" && Array.isArray(value.value)) {
        const result = new Map();
        seen.set(value, result);
        for (const pair of value.value) {
          if (Array.isArray(pair) && pair.length === 2)
            result.set(__swRevive(pair[0], seen), __swRevive(pair[1], seen));
        }
        return result;
      }
      if (value[__swTag] === "set" && Array.isArray(value.value)) {
        const result = new Set();
        seen.set(value, result);
        for (const item of value.value) result.add(__swRevive(item, seen));
        return result;
      }
    }
    seen.set(value, value);
    if (Array.isArray(value)) {
      for (let i = 0; i < value.length; ++i) value[i] = __swRevive(value[i], seen);
    } else {
      const prototype = Object.getPrototypeOf(value);
      if (prototype === Object.prototype || prototype === null) {
        for (const key of keys) value[key] = __swRevive(value[key], seen);
      }
    }
    return value;
  };
  const __swWrap = (value, seen) => {
    if (!value || typeof value !== "object") return value;
    if (seen.has(value)) return seen.get(value);
    const tag = Object.prototype.toString.call(value);
    if (tag === "[object Map]") {
      const result = { [__swTag]: "map", value: [] };
      seen.set(value, result);
      for (const [key, item] of value)
        result.value.push([__swWrap(key, seen), __swWrap(item, seen)]);
      return result;
    }
    if (tag === "[object Set]") {
      const result = { [__swTag]: "set", value: [] };
      seen.set(value, result);
      for (const item of value) result.value.push(__swWrap(item, seen));
      return result;
    }
    if (Array.isArray(value)) {
      const result = [];
      seen.set(value, result);
      for (const item of value) result.push(__swWrap(item, seen));
      return result;
    }
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== Object.prototype && prototype !== null) return value;
    const result = prototype === null ? Object.create(null) : {};
    seen.set(value, result);
    for (const key of Object.keys(value)) {
      if (key === "__proto__") continue;
      try { result[key] = __swWrap(value[key], seen); } catch (_) {}
    }
    return result;
  };
  const parameters = __swArgs.map(value => __swRevive(value, new Map()));
  const value = globalThis.eval(__swExpression);
  const result = typeof value === "function" ? value(...parameters) : value;
  if (!__swReturnByValue) return result;
  if (result && typeof result === "object" && typeof result.then === "function")
    return result.then(value => __swWrap(value, new Map()));
  return __swWrap(result, new Map());
})"#;

type DialogHandler = Arc<dyn Fn(Dialog) + Send + Sync>;
type ConsoleHandler = Arc<dyn Fn(ConsoleMessage) + Send + Sync>;
type PageErrorHandler = Arc<dyn Fn(PageError) + Send + Sync>;
type FileChooserHandler = Arc<dyn Fn(FileChooser) + Send + Sync>;
type DownloadHandler = Arc<dyn Fn(Download) + Send + Sync>;
type TargetHandler = Arc<dyn Fn(Page) + Send + Sync>;

struct TargetListener {
    first_sequence: u64,
    handler: TargetHandler,
}

#[derive(Clone)]
struct TargetData {
    sequence: u64,
    type_: String,
    url: String,
    context_id: Option<String>,
    session_id: Option<String>,
    attached: bool,
    initialization: PageInitialization,
    reported: bool,
    page: Option<Page>,
    opener_id: Option<String>,
}

#[derive(Clone)]
enum PageInitialization {
    Pending,
    Ready,
    Unusable { reason: String },
}

#[derive(Clone)]
pub struct Browser {
    inner: Arc<BrowserInner>,
}

struct BrowserInner {
    connection: Connection,
    contexts: RwLock<HashMap<Option<String>, BrowserContext>>,
    // Outer None means no page target has established the default CDP context yet.
    default_cdp_context_id: RwLock<Option<Option<String>>>,
    targets: RwLock<HashMap<String, TargetData>>,
    closed_targets: Mutex<HashSet<String>>,
    next_target_sequence: AtomicU64,
    target_handlers: Mutex<Vec<TargetListener>>,
    pages: RwLock<HashMap<String, Page>>,
    sessions: RwLock<HashMap<String, String>>,
    session_targets: RwLock<HashMap<String, String>>,
    notify: Notify,
    closed: Mutex<bool>,
    download_handlers: Mutex<Vec<DownloadHandler>>,
    downloads: Mutex<HashMap<String, Download>>,
    download_paths: Mutex<HashMap<Option<String>, PathBuf>>,
}

#[derive(Clone)]
pub struct BrowserContext {
    browser: Weak<BrowserInner>,
    id: Option<String>,
}

#[derive(Clone)]
pub struct Page {
    pub(crate) inner: Arc<PageInner>,
}

pub(crate) struct PageInner {
    browser: Weak<BrowserInner>,
    target_id: String,
    context_id: Option<String>,
    main_session: String,
    state: Mutex<PageState>,
    notify: Notify,
    dialog_handlers: Mutex<Vec<DialogHandler>>,
    console_handlers: Mutex<Vec<ConsoleHandler>>,
    page_error_handlers: Mutex<Vec<PageErrorHandler>>,
    file_chooser_handlers: Mutex<Vec<FileChooserHandler>>,
    download_handlers: Mutex<Vec<DownloadHandler>>,
    input: Arc<InputState>,
}

struct PageState {
    frames: HashMap<String, FrameData>,
    main_frame: Option<String>,
    next_seq: u32,
    ready: bool,
    initialization_error: Option<String>,
    closed: bool,
    default_timeout: Duration,
    default_navigation_timeout: Duration,
    viewport: Option<ViewportSize>,
    navigation_generation: u64,
    inflight: HashSet<String>,
    last_network_activity: Instant,
    loader_requests: HashMap<String, String>,
    responses: HashMap<String, Response>,
    failures: HashMap<String, String>,
    network_sequence: u64,
    request_events: Vec<(u64, Request)>,
    response_events: Vec<(u64, Response)>,
}

#[derive(Clone)]
struct FrameData {
    parent: Option<String>,
    url: String,
    name: String,
    seq: u32,
    detached: bool,
    owner_session: String,
    contexts: HashMap<World, ContextData>,
    lifecycle: HashSet<LoadState>,
    loader_id: Option<String>,
}

#[derive(Clone, Default)]
struct ContextData {
    id: u64,
    utility_object: Option<String>,
    injected_object: Option<String>,
}

#[derive(Clone)]
pub struct Frame {
    pub(crate) page: Page,
    pub(crate) id: String,
}

#[derive(Clone)]
pub struct JsHandle {
    pub(crate) page: Page,
    pub(crate) remote_object: Value,
    pub(crate) session_id: String,
    pub(crate) context_id: u64,
    pub(crate) frame_id: String,
    disposed: Arc<Mutex<bool>>,
}

#[derive(Clone)]
pub struct ElementHandle(pub JsHandle);

#[derive(Clone)]
pub struct Dialog {
    session: Session,
    pub type_: DialogType,
    pub message: String,
    pub default_value: String,
    handled: Arc<Mutex<bool>>,
}

#[derive(Clone)]
pub struct FileChooser {
    pub element: ElementHandle,
    pub is_multiple: bool,
}

#[derive(Clone)]
pub struct Download {
    pub url: String,
    pub suggested_filename: String,
    guid: String,
    download_path: Option<PathBuf>,
    state: Arc<Mutex<DownloadState>>,
    notify: Arc<Notify>,
}

#[derive(Default)]
struct DownloadState {
    completed: bool,
    canceled: bool,
    file_path: Option<PathBuf>,
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser").finish_non_exhaustive()
    }
}
impl std::fmt::Debug for BrowserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserContext")
            .field("id", &self.id)
            .finish()
    }
}
impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("target_id", &self.inner.target_id)
            .finish()
    }
}
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame").field("id", &self.id).finish()
    }
}
impl std::fmt::Debug for JsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsHandle")
            .field("remote_object", &self.remote_object)
            .finish()
    }
}
impl std::fmt::Debug for ElementHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ElementHandle").field(&self.0).finish()
    }
}
impl std::fmt::Debug for Dialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialog")
            .field("type_", &self.type_)
            .field("message", &self.message)
            .finish()
    }
}
impl std::fmt::Debug for FileChooser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileChooser")
            .field("is_multiple", &self.is_multiple)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("url", &self.url)
            .field("suggested_filename", &self.suggested_filename)
            .finish()
    }
}

impl Browser {
    pub async fn connect(transport: impl Transport) -> Result<Self> {
        Self::connect_connection(Connection::new(transport)).await
    }

    /// Bootstraps a browser over an existing CDP connection.
    ///
    /// Keeping the connection separate lets callers retain one-shot transports
    /// (such as inherited file descriptors) when browser bootstrap is retried.
    pub async fn connect_connection(connection: Connection) -> Result<Self> {
        let mut events = connection.events();
        let inner = Arc::new_cyclic(|weak| {
            let default = BrowserContext {
                browser: weak.clone(),
                id: None,
            };
            BrowserInner {
                connection: connection.clone(),
                contexts: RwLock::new(HashMap::from([(None, default)])),
                default_cdp_context_id: RwLock::new(None),
                targets: RwLock::new(HashMap::new()),
                closed_targets: Mutex::new(HashSet::new()),
                next_target_sequence: AtomicU64::new(1),
                target_handlers: Mutex::new(Vec::new()),
                pages: RwLock::new(HashMap::new()),
                sessions: RwLock::new(HashMap::new()),
                session_targets: RwLock::new(HashMap::new()),
                notify: Notify::new(),
                closed: Mutex::new(false),
                download_handlers: Mutex::new(Vec::new()),
                downloads: Mutex::new(HashMap::new()),
                download_paths: Mutex::new(HashMap::new()),
            }
        });
        let browser = Self { inner };
        let weak = Arc::downgrade(&browser.inner);
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                handle_event(&inner, event);
            }
        });
        browser
            .inner
            .connection
            .send("Target.setDiscoverTargets", json!({"discover": true}), None)
            .await?;
        browser
            .inner
            .connection
            .send(
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true
                }),
                None,
            )
            .await?;
        let targets = browser
            .inner
            .connection
            .send("Target.getTargets", json!({}), None)
            .await?;
        let target_infos: Vec<Value> = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        for target in &target_infos {
            update_target_info(&browser.inner, target);
        }
        let page_targets: Vec<String> = target_infos
            .iter()
            .filter(|target| is_user_page_target_info(target))
            .filter_map(|target| {
                target
                    .get("targetId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        let deadline = Deadline::new(DEFAULT_TIMEOUT);
        deadline
            .run("browser.connect", async {
                for target in page_targets {
                    loop {
                        if browser
                            .inner
                            .closed_targets
                            .lock()
                            .unwrap()
                            .contains(&target)
                        {
                            break;
                        }
                        let page = {
                            browser
                                .inner
                                .targets
                                .read()
                                .unwrap()
                                .get(&target)
                                .and_then(|target| target.page.clone())
                        };
                        if let Some(page) = page {
                            if let Err(error) = page.wait_ready(deadline).await {
                                tracing::warn!(
                                    target_id = %target,
                                    error = %error,
                                    "ignoring unusable page during browser connection"
                                );
                            }
                            break;
                        }
                        wait_for_browser_event(&browser.inner).await;
                    }
                }
                Ok(())
            })
            .await?;
        Ok(browser)
    }

    pub async fn connect_ws(url: &str) -> Result<Self> {
        let transport = WebSocketTransport::connect(url)
            .await
            .map_err(steadwright_cdp::CdpError::from)?;
        Self::connect(transport).await
    }

    pub fn contexts(&self) -> Vec<BrowserContext> {
        self.inner
            .contexts
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    pub fn default_context(&self) -> BrowserContext {
        self.inner
            .contexts
            .read()
            .unwrap()
            .get(&None)
            .unwrap()
            .clone()
    }

    pub async fn new_page(&self) -> Result<Page> {
        self.default_context().new_page().await
    }

    pub async fn close(&self) -> Result<()> {
        *self.inner.closed.lock().unwrap() = true;
        let pages: Vec<Page> = self.inner.pages.read().unwrap().values().cloned().collect();
        for page in pages {
            page.inner.state.lock().unwrap().closed = true;
            page.inner.notify.notify_waiters();
        }
        for target in self.inner.targets.write().unwrap().values_mut() {
            target.attached = false;
            target.initialization = PageInitialization::Pending;
        }
        self.inner.notify.notify_waiters();
        self.inner.connection.close().await;
        Ok(())
    }

    /// Calls `handler` for each future top-level page after it is initialized.
    pub fn on_target(&self, handler: impl Fn(Page) + Send + Sync + 'static) {
        let first_sequence = self.inner.next_target_sequence.load(Ordering::SeqCst);
        self.inner
            .target_handlers
            .lock()
            .unwrap()
            .push(TargetListener {
                first_sequence,
                handler: Arc::new(handler),
            });
    }

    pub fn on_download(&self, handler: impl Fn(Download) + Send + Sync + 'static) {
        self.inner
            .download_handlers
            .lock()
            .unwrap()
            .push(Arc::new(handler));
    }
}

async fn wait_for_browser_event(browser: &BrowserInner) {
    tokio::select! {
        _ = browser.notify.notified() => {},
        _ = tokio::time::sleep(Duration::from_millis(10)) => {},
    }
}

impl BrowserContext {
    pub fn pages(&self) -> Vec<Page> {
        let Some(browser) = self.browser.upgrade() else {
            return Vec::new();
        };
        browser
            .targets
            .read()
            .unwrap()
            .values()
            .filter(|target| {
                target.type_ == "page"
                    && !target.url.starts_with("chrome-extension://")
                    && target.context_id == self.id
                    && target.attached
                    && matches!(target.initialization, PageInitialization::Ready)
            })
            .filter_map(|target| target.page.as_ref())
            .filter(|page| !page.is_closed())
            .cloned()
            .collect()
    }

    /// Returns the page at `url`, retrying initialization if a prior attempt
    /// failed while the target and its CDP session are still attached.
    pub async fn page_for_url(&self, url: &str) -> Result<Option<Page>> {
        let Some(browser) = self.browser.upgrade() else {
            return Ok(None);
        };
        let target_id = browser
            .targets
            .read()
            .unwrap()
            .iter()
            .find(|(_, target)| {
                target.type_ == "page"
                    && !target.url.starts_with("chrome-extension://")
                    && target.context_id == self.id
                    && target.attached
                    && target.url == url
            })
            .map(|(id, _)| id.clone());
        let Some(target_id) = target_id else {
            return Ok(None);
        };

        let deadline = Deadline::new(DEFAULT_TIMEOUT);
        loop {
            let retry = {
                let mut targets = browser.targets.write().unwrap();
                let Some(target) = targets.get_mut(&target_id) else {
                    return Ok(None);
                };
                if !target.attached {
                    return Ok(None);
                }
                match &target.initialization {
                    PageInitialization::Ready => return Ok(target.page.clone()),
                    PageInitialization::Pending => None,
                    PageInitialization::Unusable { reason } => {
                        tracing::info!(
                            target_id = %target_id,
                            previous_error = %reason,
                            "retrying page initialization"
                        );
                        target.initialization = PageInitialization::Pending;
                        target.page.as_ref().map(|page| {
                            let mut state = page.inner.state.lock().unwrap();
                            state.ready = false;
                            state.initialization_error = None;
                            (page.clone(), target.session_id.clone())
                        })
                    }
                }
            };

            if let Some((page, Some(session_id))) = retry {
                initialize_session(page.clone(), session_id, true).await?;
                return Ok(Some(page));
            }
            let page = browser
                .targets
                .read()
                .unwrap()
                .get(&target_id)
                .and_then(|target| target.page.clone());
            if let Some(page) = page {
                page.wait_ready(deadline).await?;
                return Ok(Some(page));
            }
            if deadline.expired() {
                return Err(Error::timeout("context.page_for_url", deadline.timeout()));
            }
            wait_for_browser_event(&browser).await;
        }
    }

    pub async fn new_page(&self) -> Result<Page> {
        let browser = self.browser.upgrade().ok_or(Error::TargetClosed)?;
        let mut params = Map::new();
        params.insert("url".into(), json!("about:blank"));
        if let Some(id) = &self.id {
            params.insert("browserContextId".into(), json!(id));
        }
        let result = browser
            .connection
            .send("Target.createTarget", Value::Object(params), None)
            .await?;
        let target = result
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol(steadwright_cdp::CdpError::Disconnected))?
            .to_owned();
        let deadline = Deadline::new(DEFAULT_TIMEOUT);
        deadline
            .run("context.new_page", async {
                loop {
                    if browser.closed_targets.lock().unwrap().contains(&target) {
                        return Err(Error::TargetClosed);
                    }
                    let page = {
                        browser
                            .targets
                            .read()
                            .unwrap()
                            .get(&target)
                            .and_then(|target| target.page.clone())
                    };
                    if let Some(page) = page {
                        page.wait_ready(deadline).await?;
                        return Ok(page);
                    }
                    wait_change(&browser.notify).await;
                }
            })
            .await
    }

    pub async fn set_download_behavior(&self, path: impl Into<PathBuf>) -> Result<()> {
        let browser = self.browser.upgrade().ok_or(Error::TargetClosed)?;
        let path = path.into();
        let mut params =
            json!({"behavior":"allowAndName", "downloadPath":path, "eventsEnabled":true});
        if let Some(id) = &self.id {
            params["browserContextId"] = json!(id);
        }
        browser
            .connection
            .send("Browser.setDownloadBehavior", params, None)
            .await?;
        browser
            .download_paths
            .lock()
            .unwrap()
            .insert(self.id.clone(), path);
        Ok(())
    }
}

impl Drop for BrowserInner {
    fn drop(&mut self) {
        *self.closed.lock().unwrap() = true;
    }
}

impl Page {
    fn new(
        browser: &Arc<BrowserInner>,
        target_id: String,
        context_id: Option<String>,
        session_id: String,
    ) -> Self {
        let initial_frame = FrameData {
            parent: None,
            url: String::new(),
            name: String::new(),
            seq: 0,
            detached: false,
            owner_session: session_id.clone(),
            contexts: HashMap::new(),
            lifecycle: HashSet::from([LoadState::Commit]),
            loader_id: None,
        };
        Self {
            inner: Arc::new(PageInner {
                browser: Arc::downgrade(browser),
                target_id: target_id.clone(),
                context_id,
                main_session: session_id,
                state: Mutex::new(PageState {
                    frames: HashMap::from([(target_id.clone(), initial_frame)]),
                    main_frame: Some(target_id),
                    next_seq: 1,
                    ready: false,
                    initialization_error: None,
                    closed: false,
                    default_timeout: DEFAULT_TIMEOUT,
                    default_navigation_timeout: DEFAULT_TIMEOUT,
                    viewport: None,
                    navigation_generation: 0,
                    inflight: HashSet::new(),
                    last_network_activity: Instant::now(),
                    loader_requests: HashMap::new(),
                    responses: HashMap::new(),
                    failures: HashMap::new(),
                    network_sequence: 0,
                    request_events: Vec::new(),
                    response_events: Vec::new(),
                }),
                notify: Notify::new(),
                dialog_handlers: Mutex::new(Vec::new()),
                console_handlers: Mutex::new(Vec::new()),
                page_error_handlers: Mutex::new(Vec::new()),
                file_chooser_handlers: Mutex::new(Vec::new()),
                download_handlers: Mutex::new(Vec::new()),
                input: Arc::new(InputState::default()),
            }),
        }
    }

    async fn wait_ready(&self, deadline: Deadline) -> Result<()> {
        deadline
            .run("context.new_page", async {
                loop {
                    let (closed, ready, initialization_error) = {
                        let state = self.inner.state.lock().unwrap();
                        (
                            state.closed,
                            state.ready,
                            state.initialization_error.clone(),
                        )
                    };
                    if closed {
                        return Err(Error::TargetClosed);
                    }
                    if let Some(error) = initialization_error {
                        return Err(Error::Navigation(error));
                    }
                    if ready {
                        return Ok(());
                    }
                    wait_change(&self.inner.notify).await;
                }
            })
            .await
    }

    fn browser(&self) -> Result<Arc<BrowserInner>> {
        self.inner.browser.upgrade().ok_or(Error::TargetClosed)
    }
    pub(crate) fn default_timeout(&self) -> Duration {
        self.inner.state.lock().unwrap().default_timeout
    }
    fn navigation_timeout(&self) -> Duration {
        self.inner.state.lock().unwrap().default_navigation_timeout
    }
    pub(crate) fn deadline(&self, timeout: Option<Duration>, navigation: bool) -> Deadline {
        Deadline::new(timeout.unwrap_or_else(|| {
            if navigation {
                self.navigation_timeout()
            } else {
                self.default_timeout()
            }
        }))
    }

    pub fn main_frame(&self) -> Frame {
        let id = self
            .inner
            .state
            .lock()
            .unwrap()
            .main_frame
            .clone()
            .unwrap_or_else(|| self.inner.target_id.clone());
        Frame {
            page: self.clone(),
            id,
        }
    }
    pub(crate) fn main_session_id(&self) -> &str {
        &self.inner.main_session
    }
    pub(crate) fn navigation_generation(&self) -> u64 {
        self.inner.state.lock().unwrap().navigation_generation
    }
    pub(crate) async fn wait_for_action_navigation(
        &self,
        before: u64,
        deadline: Deadline,
    ) -> Result<()> {
        // CDP has no single "navigation scheduled" event. Give events queued by the
        // input dispatch a brief chance to commit, then reuse the normal load waiter.
        let signal_deadline = Instant::now() + Duration::from_millis(50);
        loop {
            if self.navigation_generation() > before {
                return self
                    .wait_for_load_state(LoadState::Load, Some(deadline.remaining()))
                    .await;
            }
            if Instant::now() >= signal_deadline || deadline.expired() {
                return Ok(());
            }
            tokio::select! {
                _ = self.inner.notify.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            }
        }
    }
    pub fn frames(&self) -> Vec<Frame> {
        self.inner
            .state
            .lock()
            .unwrap()
            .frames
            .iter()
            .filter(|(_, f)| !f.detached)
            .map(|(id, _)| Frame {
                page: self.clone(),
                id: id.clone(),
            })
            .collect()
    }
    pub fn url(&self) -> String {
        self.main_frame().url()
    }
    pub fn is_closed(&self) -> bool {
        self.inner.state.lock().unwrap().closed
    }
    pub fn viewport_size(&self) -> Option<ViewportSize> {
        self.inner.state.lock().unwrap().viewport
    }
    pub fn keyboard(&self) -> Keyboard {
        Keyboard::new(self.clone(), self.inner.input.clone())
    }
    pub fn mouse(&self) -> Mouse {
        Mouse::new(self.clone(), self.inner.input.clone())
    }
    pub fn touchscreen(&self) -> Touchscreen {
        Touchscreen::new(self.clone(), self.inner.input.clone())
    }
    pub fn set_default_timeout(&self, timeout: impl IntoTimeout) {
        self.inner.state.lock().unwrap().default_timeout = timeout.into_timeout();
    }
    pub fn set_default_navigation_timeout(&self, timeout: impl IntoTimeout) {
        self.inner.state.lock().unwrap().default_navigation_timeout = timeout.into_timeout();
    }

    pub async fn goto(&self, url: &str, options: GotoOptions) -> Result<Option<Response>> {
        self.main_frame().goto(url, options).await
    }
    pub async fn evaluate(&self, expression: &str, arg: impl Into<CallArg>) -> Result<JsValue> {
        self.main_frame().evaluate(expression, arg).await
    }
    pub async fn evaluate_json(&self, expression: &str, arg: impl Into<CallArg>) -> Result<Value> {
        Ok(self.evaluate(expression, arg).await?.to_json())
    }
    pub async fn evaluate_handle(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
    ) -> Result<JsHandle> {
        self.main_frame().evaluate_handle(expression, arg).await
    }
    pub async fn title(&self) -> Result<String> {
        Ok(self
            .main_frame()
            .evaluate_utility_json("document.title", Value::Null)
            .await?
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }
    pub async fn content(&self) -> Result<String> {
        let value = self.main_frame().evaluate_utility_json("() => { let result = ''; if (document.doctype) result = new XMLSerializer().serializeToString(document.doctype); if (document.documentElement) result += document.documentElement.outerHTML; return result; }", Value::Null).await?;
        Ok(value.as_str().unwrap_or_default().to_owned())
    }
    pub async fn wait_for_timeout(&self, milliseconds: u64) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(milliseconds)).await;
        Ok(())
    }
    pub async fn wait_for_load_state(
        &self,
        state: LoadState,
        timeout: Option<Duration>,
    ) -> Result<()> {
        let deadline = self.deadline(timeout, true);
        let frame = self.main_frame();
        deadline
            .run("page.wait_for_load_state", async {
                loop {
                    let ready = {
                        let page_state = self.inner.state.lock().unwrap();
                        if state == LoadState::NetworkIdle {
                            page_state.inflight.is_empty()
                                && page_state.last_network_activity.elapsed()
                                    >= Duration::from_millis(500)
                        } else {
                            page_state
                                .frames
                                .get(&frame.id)
                                .is_some_and(|f| f.lifecycle.contains(&state))
                        }
                    };
                    if ready {
                        return Ok(());
                    }
                    if self.is_closed() {
                        return Err(Error::TargetClosed);
                    }
                    if state == LoadState::NetworkIdle {
                        tokio::select! {
                            _ = self.inner.notify.notified() => {},
                            _ = tokio::time::sleep(Duration::from_millis(25)) => {},
                        }
                    } else {
                        wait_change(&self.inner.notify).await;
                    }
                }
            })
            .await
    }
    pub async fn wait_for_url(
        &self,
        matcher: UrlMatcher,
        options: WaitForUrlOptions,
    ) -> Result<()> {
        let deadline = self.deadline(options.timeout, true);
        deadline
            .run("page.wait_for_url", async {
                loop {
                    if matcher.matches(&self.url()) {
                        return self
                            .wait_for_load_state(options.wait_until, Some(deadline.remaining()))
                            .await;
                    }
                    if self.is_closed() {
                        return Err(Error::TargetClosed);
                    }
                    wait_change(&self.inner.notify).await;
                }
            })
            .await
    }
    /// Registers synchronously, so constructing this future before an action cannot miss a response.
    pub fn wait_for_response(
        &self,
        matcher: UrlMatcher,
        timeout: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<Response>> + Send + 'static {
        let page = self.clone();
        let first_sequence = self.inner.state.lock().unwrap().network_sequence + 1;
        let deadline = self.deadline(timeout, false);
        async move {
            deadline
                .run("page.wait_for_response", async {
                    loop {
                        let response = {
                            let state = page.inner.state.lock().unwrap();
                            state
                                .response_events
                                .iter()
                                .find(|(sequence, response)| {
                                    *sequence >= first_sequence && matcher.matches(&response.url)
                                })
                                .map(|(_, response)| response.clone())
                        };
                        if let Some(response) = response {
                            return Ok(response);
                        }
                        if page.is_closed() {
                            return Err(Error::TargetClosed);
                        }
                        wait_change(&page.inner.notify).await;
                    }
                })
                .await
        }
    }

    /// Registers synchronously, so constructing this future before an action cannot miss a request.
    pub fn wait_for_request(
        &self,
        matcher: UrlMatcher,
        timeout: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<Request>> + Send + 'static {
        let page = self.clone();
        let first_sequence = self.inner.state.lock().unwrap().network_sequence + 1;
        let deadline = self.deadline(timeout, false);
        async move {
            deadline
                .run("page.wait_for_request", async {
                    loop {
                        let request = {
                            let state = page.inner.state.lock().unwrap();
                            state
                                .request_events
                                .iter()
                                .find(|(sequence, request)| {
                                    *sequence >= first_sequence && matcher.matches(&request.url)
                                })
                                .map(|(_, request)| request.clone())
                        };
                        if let Some(request) = request {
                            return Ok(request);
                        }
                        if page.is_closed() {
                            return Err(Error::TargetClosed);
                        }
                        wait_change(&page.inner.notify).await;
                    }
                })
                .await
        }
    }
    pub async fn set_viewport_size(&self, width: u32, height: u32) -> Result<()> {
        self.send_main(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width":width,"height":height,"deviceScaleFactor":0,"mobile":false
            }),
        )
        .await?;
        self.inner.state.lock().unwrap().viewport = Some(ViewportSize { width, height });
        Ok(())
    }
    pub async fn bring_to_front(&self) -> Result<()> {
        self.send_main("Page.bringToFront", json!({})).await?;
        Ok(())
    }
    pub async fn close(&self) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        self.browser()?
            .connection
            .send(
                "Target.closeTarget",
                json!({"targetId":self.inner.target_id}),
                None,
            )
            .await?;
        self.inner.state.lock().unwrap().closed = true;
        self.inner.notify.notify_waiters();
        Ok(())
    }

    pub async fn reload(&self, options: GotoOptions) -> Result<Option<Response>> {
        let before = self.inner.state.lock().unwrap().navigation_generation;
        self.send_main("Page.reload", json!({})).await?;
        self.wait_navigation_after(before, options.wait_until, options.timeout, "page.reload")
            .await?;
        Ok(self.current_response())
    }
    pub async fn go_back(&self, options: GotoOptions) -> Result<Option<Response>> {
        self.go_history(-1, options).await
    }
    pub async fn go_forward(&self, options: GotoOptions) -> Result<Option<Response>> {
        self.go_history(1, options).await
    }
    async fn go_history(&self, delta: isize, options: GotoOptions) -> Result<Option<Response>> {
        let history = self
            .send_main("Page.getNavigationHistory", json!({}))
            .await?;
        let index = history
            .get("currentIndex")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + delta as i64;
        let Some(entry) = history
            .get("entries")
            .and_then(Value::as_array)
            .and_then(|v| v.get(index as usize))
        else {
            return Ok(None);
        };
        let entry_id = entry
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::Navigation("invalid navigation history entry".into()))?;
        let (before, before_loader) = {
            let state = self.inner.state.lock().unwrap();
            let loader = state
                .main_frame
                .as_ref()
                .and_then(|id| state.frames.get(id))
                .and_then(|frame| frame.loader_id.clone());
            (state.navigation_generation, loader)
        };
        self.send_main("Page.navigateToHistoryEntry", json!({"entryId":entry_id}))
            .await?;
        self.wait_navigation_after(
            before,
            options.wait_until,
            options.timeout,
            if delta < 0 {
                "page.go_back"
            } else {
                "page.go_forward"
            },
        )
        .await?;
        let current_loader = {
            let state = self.inner.state.lock().unwrap();
            state
                .main_frame
                .as_ref()
                .and_then(|id| state.frames.get(id))
                .and_then(|frame| frame.loader_id.clone())
        };
        if current_loader == before_loader {
            Ok(None)
        } else {
            Ok(self.current_response())
        }
    }
    async fn wait_navigation_after(
        &self,
        before: u64,
        state: LoadState,
        timeout: Option<Duration>,
        api: &str,
    ) -> Result<()> {
        let deadline = self.deadline(timeout, true);
        deadline
            .run(api, async {
                loop {
                    let ready = {
                        let s = self.inner.state.lock().unwrap();
                        s.navigation_generation > before
                            && s.main_frame
                                .as_ref()
                                .and_then(|id| s.frames.get(id))
                                .is_some_and(|f| f.lifecycle.contains(&state))
                    };
                    if ready {
                        return Ok(());
                    }
                    wait_change(&self.inner.notify).await;
                }
            })
            .await
    }
    fn current_response(&self) -> Option<Response> {
        let s = self.inner.state.lock().unwrap();
        let loader = s
            .main_frame
            .as_ref()
            .and_then(|id| s.frames.get(id))
            .and_then(|f| f.loader_id.as_ref())?;
        let request = s.loader_requests.get(loader)?;
        s.responses.get(request).cloned()
    }

    pub async fn screenshot(&self, options: ScreenshotOptions) -> Result<Vec<u8>> {
        if options.format == ScreenshotFormat::Png && options.quality.is_some() {
            return Err(Error::InvalidArgument(
                "quality is unsupported for PNG screenshots".into(),
            ));
        }
        if options.quality.is_some_and(|q| q > 100) {
            return Err(Error::InvalidArgument(
                "quality must be between 0 and 100".into(),
            ));
        }
        let deadline = self.deadline(options.timeout, false);
        deadline
            .run("page.screenshot", self.screenshot_inner(options))
            .await
    }
    async fn screenshot_inner(&self, options: ScreenshotOptions) -> Result<Vec<u8>> {
        if options.omit_background {
            self.send_main(
                "Emulation.setDefaultBackgroundColorOverride",
                json!({"color":{"r":0,"g":0,"b":0,"a":0}}),
            )
            .await?;
        }
        let result = async {
            let mut params = json!({"format":match options.format { ScreenshotFormat::Png => "png", ScreenshotFormat::Jpeg => "jpeg" }, "captureBeyondViewport":options.full_page});
            if let Some(q) = options.quality { params["quality"] = json!(q); }
            let metrics = self.send_main("Page.getLayoutMetrics", json!({})).await?;
            let clip = if options.full_page {
                let size = metrics.get("cssContentSize").ok_or_else(|| Error::Navigation("Page.getLayoutMetrics did not return cssContentSize".into()))?;
                Some(json!({"x":0,"y":0,"width":size["width"],"height":size["height"],"scale":1}))
            } else { options.clip.map(|c| json!({"x":c.x,"y":c.y,"width":c.width,"height":c.height,"scale":1})) };
            if let Some(mut clip) = clip {
                if options.scale == ScreenshotScale::Css {
                    let content = metrics.get("contentSize").and_then(|v| v.get("width")).and_then(Value::as_f64).unwrap_or(1.0);
                    let css = metrics.get("cssContentSize").and_then(|v| v.get("width")).and_then(Value::as_f64).unwrap_or(content);
                    clip["scale"] = json!(1.0 / (content / css).max(1.0));
                }
                params["clip"] = clip;
            }
            let response = self.send_main("Page.captureScreenshot", params).await?;
            let data = response.get("data").and_then(Value::as_str).ok_or_else(|| Error::Navigation("Page.captureScreenshot returned no data".into()))?;
            base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| Error::InvalidArgument(e.to_string()))
        }.await;
        if options.omit_background {
            let _ = self
                .send_main("Emulation.setDefaultBackgroundColorOverride", json!({}))
                .await;
        }
        result
    }

    pub fn on_dialog(&self, handler: impl Fn(Dialog) + Send + Sync + 'static) {
        self.inner
            .dialog_handlers
            .lock()
            .unwrap()
            .push(Arc::new(handler));
    }
    pub fn on_console(&self, handler: impl Fn(ConsoleMessage) + Send + Sync + 'static) {
        self.inner
            .console_handlers
            .lock()
            .unwrap()
            .push(Arc::new(handler));
    }
    pub fn on_page_error(&self, handler: impl Fn(PageError) + Send + Sync + 'static) {
        self.inner
            .page_error_handlers
            .lock()
            .unwrap()
            .push(Arc::new(handler));
    }
    pub fn on_file_chooser(&self, handler: impl Fn(FileChooser) + Send + Sync + 'static) {
        self.inner
            .file_chooser_handlers
            .lock()
            .unwrap()
            .push(Arc::new(handler));
    }
    pub fn on_download(&self, handler: impl Fn(Download) + Send + Sync + 'static) {
        self.inner
            .download_handlers
            .lock()
            .unwrap()
            .push(Arc::new(handler));
    }

    /// Registers before returning the future, preserving the wait-before-action pattern.
    pub fn wait_for_event(
        &self,
        event: &str,
        timeout: Option<Duration>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<PageEvent>> + Send + 'static>>
    {
        let deadline = self.deadline(timeout, false);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        match event {
            "download" => self.on_download(move |download| {
                let _ = sender.send(PageEvent::Download(download));
            }),
            "dialog" => self.on_dialog(move |dialog| {
                if sender.send(PageEvent::Dialog(dialog.clone())).is_err() {
                    tokio::spawn(async move {
                        let _ = dialog.dismiss().await;
                    });
                }
            }),
            "filechooser" => self.on_file_chooser(move |chooser| {
                let _ = sender.send(PageEvent::FileChooser(chooser));
            }),
            "popup" => match self.browser() {
                Ok(browser) => {
                    let source_id = self.inner.target_id.clone();
                    let filter = browser.clone();
                    Browser { inner: browser }.on_target(move |popup| {
                        let is_popup = filter
                            .targets
                            .read()
                            .unwrap()
                            .get(&popup.inner.target_id)
                            .is_some_and(|target| target.opener_id.as_deref() == Some(&source_id));
                        if is_popup {
                            let _ = sender.send(PageEvent::Popup(popup));
                        }
                    });
                }
                Err(error) => return Box::pin(async move { Err(error) }),
            },
            _ => {
                let event = event.to_owned();
                return Box::pin(async move {
                    Err(Error::InvalidArgument(format!(
                        "Unsupported page event: {event}"
                    )))
                });
            }
        }
        Box::pin(async move {
            deadline
                .run("page.wait_for_event", async {
                    receiver.recv().await.ok_or(Error::TargetClosed)
                })
                .await
        })
    }

    pub(crate) async fn send_main(&self, method: &str, params: Value) -> Result<Value> {
        let browser = self.browser()?;
        browser
            .connection
            .session(self.inner.main_session.clone())
            .send(method, params)
            .await
            .map_err(Into::into)
    }
    pub(crate) async fn send_session(
        &self,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.browser()?
            .connection
            .session(session.to_owned())
            .send(method, params)
            .await
            .map_err(Into::into)
    }
}

impl Frame {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn url(&self) -> String {
        self.data().map(|v| v.url).unwrap_or_default()
    }
    pub fn name(&self) -> String {
        self.data().map(|v| v.name).unwrap_or_default()
    }
    pub fn is_detached(&self) -> bool {
        self.data().is_none_or(|v| v.detached)
    }
    pub(crate) fn owner_session(&self) -> Result<String> {
        self.data()
            .map(|frame| frame.owner_session)
            .ok_or(Error::FrameDetached)
    }
    pub(crate) fn same_frame(&self, other: &Frame) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.page.inner, &other.page.inner)
    }
    pub(crate) fn frame_by_sequence(&self, sequence: u32) -> Option<Frame> {
        self.page
            .inner
            .state
            .lock()
            .unwrap()
            .frames
            .iter()
            .find(|(_, frame)| frame.seq == sequence && !frame.detached)
            .map(|(id, _)| Frame {
                page: self.page.clone(),
                id: id.clone(),
            })
    }
    pub(crate) fn frame_by_id(&self, frame_id: &str) -> Option<Frame> {
        self.page
            .inner
            .state
            .lock()
            .unwrap()
            .frames
            .get(frame_id)
            .filter(|frame| !frame.detached)
            .map(|_| Frame {
                page: self.page.clone(),
                id: frame_id.to_owned(),
            })
    }
    pub fn parent_frame(&self) -> Option<Frame> {
        self.data()?.parent.map(|id| Frame {
            page: self.page.clone(),
            id,
        })
    }
    pub fn child_frames(&self) -> Vec<Frame> {
        self.page
            .inner
            .state
            .lock()
            .unwrap()
            .frames
            .iter()
            .filter(|(_, frame)| frame.parent.as_deref() == Some(&self.id) && !frame.detached)
            .map(|(id, _)| Frame {
                page: self.page.clone(),
                id: id.clone(),
            })
            .collect()
    }
    fn data(&self) -> Option<FrameData> {
        self.page
            .inner
            .state
            .lock()
            .unwrap()
            .frames
            .get(&self.id)
            .cloned()
    }

    pub async fn context(&self, world: World) -> Result<u64> {
        self.context_with_timeout(world, None).await
    }

    async fn context_with_timeout(&self, world: World, timeout: Option<Duration>) -> Result<u64> {
        let deadline = self.page.deadline(timeout, false);
        deadline
            .run("frame.context", async {
                loop {
                    match self.data() {
                        None => return Err(Error::FrameDetached),
                        Some(frame) if frame.detached => return Err(Error::FrameDetached),
                        Some(frame) if frame.contexts.get(&world).is_some_and(|c| c.id != 0) => {
                            return Ok(frame.contexts[&world].id);
                        }
                        _ => wait_change(&self.page.inner.notify).await,
                    }
                }
            })
            .await
    }

    pub async fn injected(&self, world: World) -> Result<JsHandle> {
        let (context, session, cached, seq) = self.context_snapshot(world).await?;
        if let Some(object_id) = cached.and_then(|c| c.injected_object) {
            return Ok(self.make_handle(json!({"objectId": object_id}), session, context));
        }
        let source = format!(
            "(() => {{ const module = {{ exports: {{}} }}; {PLAYWRIGHT_INJECTED}; return new (module.exports.InjectedScript())(globalThis, {{isUnderTest:false,sdkLanguage:'javascript',frameSeq:{seq},testIdAttributeName:'data-testid',stableRafCount:1,browserName:'chromium',shouldPrependErrorPrefix:true,isUtilityWorld:true,customEngines:[]}}); }})()"
        );
        let result = self
            .page
            .send_session(
                &session,
                "Runtime.evaluate",
                json!({"contextId":context,"expression":source,"returnByValue":false}),
            )
            .await?;
        check_exception(&result)?;
        let object_id = remote_object(&result)
            .get("objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Evaluation("InjectedScript did not produce an object".into()))?
            .to_owned();
        if let Some(frame) = self
            .page
            .inner
            .state
            .lock()
            .unwrap()
            .frames
            .get_mut(&self.id)
        {
            if let Some(data) = frame.contexts.get_mut(&world) {
                if data.id == context {
                    data.injected_object = Some(object_id.clone());
                }
            }
        }
        Ok(self.make_handle(remote_object(&result).clone(), session, context))
    }

    pub async fn call_injected(
        &self,
        world: World,
        declaration: &str,
        args: Vec<CallArg>,
    ) -> Result<Value> {
        let handle = self.injected(world).await?;
        self.call_function(
            &handle.session_id,
            handle.context_id,
            handle.object_id(),
            declaration,
            args,
            true,
        )
        .await
        .map(|v| v.0)
    }
    pub async fn call_injected_handle(
        &self,
        world: World,
        declaration: &str,
        args: Vec<CallArg>,
    ) -> Result<JsHandle> {
        let handle = self.injected(world).await?;
        let (_, remote) = self
            .call_function(
                &handle.session_id,
                handle.context_id,
                handle.object_id(),
                declaration,
                args,
                false,
            )
            .await?;
        Ok(self.make_handle(remote, handle.session_id, handle.context_id))
    }

    pub async fn evaluate(&self, expression: &str, arg: impl Into<CallArg>) -> Result<JsValue> {
        self.evaluate_impl(expression, vec![arg.into()], true)
            .await
            .map(|v| v.0)
    }
    pub async fn evaluate_json(&self, expression: &str, arg: impl Into<CallArg>) -> Result<Value> {
        Ok(self.evaluate(expression, arg).await?.to_json())
    }
    pub(crate) async fn evaluate_utility(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
    ) -> Result<JsValue> {
        let (context, session, _, _) = self.context_snapshot(World::Utility).await?;
        self.evaluate_with_context(expression, vec![arg.into()], true, context, &session)
            .await
            .map(|value| value.0)
    }
    pub(crate) async fn evaluate_utility_json(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
    ) -> Result<Value> {
        Ok(self.evaluate_utility(expression, arg).await?.to_json())
    }
    pub async fn evaluate_handle(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
    ) -> Result<JsHandle> {
        let (context, session, _, _) = self.context_snapshot(World::Main).await?;
        let (_, remote) = self
            .evaluate_with_context(expression, vec![arg.into()], false, context, &session)
            .await?;
        Ok(self.make_handle(remote, session, context))
    }
    pub(crate) async fn evaluate_handle_args(
        &self,
        expression: &str,
        args: Vec<CallArg>,
    ) -> Result<JsHandle> {
        let (context, session, _, _) = self.context_snapshot(World::Main).await?;
        let (_, remote) = self
            .evaluate_with_context(expression, args, false, context, &session)
            .await?;
        Ok(self.make_handle(remote, session, context))
    }
    async fn evaluate_impl(
        &self,
        expression: &str,
        args: Vec<CallArg>,
        return_by_value: bool,
    ) -> Result<(JsValue, Value)> {
        let (context, session, _, _) = self.context_snapshot(World::Main).await?;
        self.evaluate_with_context(expression, args, return_by_value, context, &session)
            .await
    }
    async fn evaluate_with_context(
        &self,
        expression: &str,
        args: Vec<CallArg>,
        return_by_value: bool,
        context: u64,
        session: &str,
    ) -> Result<(JsValue, Value)> {
        let utility = self.utility_object(context, session).await?;
        let expression = normalize_expression(expression);
        let mut values = vec![
            Value::Bool(true),
            json!(return_by_value),
            Value::String(EVALUATION_WRAPPER.to_owned()),
            json!(args.len() + 2),
            Value::String(expression),
            Value::Bool(return_by_value),
        ];
        let mut handles = Vec::new();
        for arg in args {
            push_serialized(&mut values, &mut handles, arg, context, session)?;
        }
        let mut call_args: Vec<Value> = std::iter::once(json!({"objectId":utility}))
            .chain(values.into_iter().map(|value| json!({"value":value})))
            .collect();
        call_args.extend(handles);
        let result = self.page.send_session(session, "Runtime.callFunctionOn", json!({
            "functionDeclaration":"(utilityScript, ...args) => utilityScript.evaluate(...args)",
            "objectId":utility,"arguments":call_args,"returnByValue":return_by_value,"awaitPromise":true,"userGesture":true
        })).await?;
        check_exception(&result)?;
        let remote = remote_object(&result);
        if return_by_value {
            Ok((decode_serialized_remote(remote), Value::Null))
        } else {
            Ok((JsValue::Undefined, remote.clone()))
        }
    }

    async fn utility_object(&self, context: u64, session: &str) -> Result<String> {
        if let Some(cached) = self
            .page
            .inner
            .state
            .lock()
            .unwrap()
            .frames
            .get(&self.id)
            .and_then(|f| f.contexts.values().find(|c| c.id == context))
            .and_then(|c| c.utility_object.clone())
        {
            return Ok(cached);
        }
        let source = format!(
            "(() => {{ const module = {{ exports: {{}} }}; {PLAYWRIGHT_UTILITY}; return new (module.exports.UtilityScript())(globalThis, false); }})()"
        );
        let result = self
            .page
            .send_session(
                session,
                "Runtime.evaluate",
                json!({"contextId":context,"expression":source,"returnByValue":false}),
            )
            .await?;
        check_exception(&result)?;
        let object = remote_object(&result)
            .get("objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Evaluation("UtilityScript did not produce an object".into()))?
            .to_owned();
        if let Some(frame) = self
            .page
            .inner
            .state
            .lock()
            .unwrap()
            .frames
            .get_mut(&self.id)
        {
            if let Some(data) = frame.contexts.values_mut().find(|c| c.id == context) {
                data.utility_object = Some(object.clone());
            }
        }
        Ok(object)
    }

    async fn context_snapshot(
        &self,
        world: World,
    ) -> Result<(u64, String, Option<ContextData>, u32)> {
        let context = self.context_with_timeout(world, None).await?;
        let frame = self.data().ok_or(Error::FrameDetached)?;
        Ok((
            context,
            frame.owner_session,
            frame.contexts.get(&world).cloned(),
            frame.seq,
        ))
    }
    pub(crate) fn make_handle(
        &self,
        remote_object: Value,
        session_id: String,
        context_id: u64,
    ) -> JsHandle {
        JsHandle {
            page: self.page.clone(),
            remote_object,
            session_id,
            context_id,
            frame_id: self.id.clone(),
            disposed: Arc::new(Mutex::new(false)),
        }
    }

    async fn call_function(
        &self,
        session: &str,
        context: u64,
        this_object: Option<&str>,
        declaration: &str,
        args: Vec<CallArg>,
        return_by_value: bool,
    ) -> Result<(Value, Value)> {
        let mut arguments = Vec::new();
        if let Some(object_id) = this_object {
            arguments.push(json!({"objectId":object_id}));
        }
        for arg in args {
            match arg {
                CallArg::Value(value) => arguments.push(json!({"value":value.to_json()})),
                CallArg::Undefined => arguments.push(json!({})),
                CallArg::Handle(handle) => {
                    validate_handle(&handle, context, session)?;
                    arguments.push(handle_call_argument(&handle)?);
                }
            }
        }
        let result = self.page.send_session(session, "Runtime.callFunctionOn", json!({"functionDeclaration":declaration,"objectId":this_object,"arguments":arguments,"returnByValue":return_by_value,"awaitPromise":true,"userGesture":true})).await?;
        check_exception(&result)?;
        let remote = remote_object(&result);
        Ok((remote_value(remote), remote.clone()))
    }

    pub async fn goto(&self, url: &str, options: GotoOptions) -> Result<Option<Response>> {
        if self.is_detached() {
            return Err(Error::FrameDetached);
        }
        let deadline = self.page.deadline(options.timeout, true);
        let before = self.page.inner.state.lock().unwrap().navigation_generation;
        let session = self.data().ok_or(Error::FrameDetached)?.owner_session;
        let mut params = json!({"url":url,"frameId":self.id,"referrerPolicy":"unsafeUrl"});
        if let Some(referer) = options.referer {
            params["referrer"] = json!(referer);
        }
        let response = self
            .page
            .send_session(&session, "Page.navigate", params)
            .await?;
        if let Some(error) = response.get("errorText").and_then(Value::as_str) {
            return Err(Error::Navigation(format!("page.goto: {error} at {url}")));
        }
        let loader = response
            .get("loaderId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        deadline.run("page.goto", async {
            loop {
                let status = {
                    let s = self.page.inner.state.lock().unwrap();
                    let failure = loader.as_ref().and_then(|id| s.loader_requests.get(id)).and_then(|id| s.failures.get(id)).cloned();
                    let frame = s.frames.get(&self.id);
                    let committed = s.navigation_generation > before || loader.as_ref().is_some_and(|id| frame.and_then(|f| f.loader_id.as_ref()) == Some(id));
                    let loaded = match options.wait_until {
                        LoadState::NetworkIdle => committed && s.inflight.is_empty() && s.last_network_activity.elapsed() >= Duration::from_millis(500),
                        state => committed && frame.is_some_and(|f| f.lifecycle.contains(&state)),
                    };
                    (failure, loaded)
                };
                if let Some(error) = status.0 { return Err(Error::Navigation(format!("page.goto: {error} at {url}"))); }
                if status.1 { return Ok(()); }
                if options.wait_until == LoadState::NetworkIdle {
                    tokio::select! { _ = self.page.inner.notify.notified() => {}, _ = tokio::time::sleep(Duration::from_millis(25)) => {} }
                } else { wait_change(&self.page.inner.notify).await; }
            }
        }).await?;
        let s = self.page.inner.state.lock().unwrap();
        let response = loader
            .as_ref()
            .and_then(|id| s.loader_requests.get(id))
            .and_then(|id| s.responses.get(id))
            .cloned();
        Ok(response)
    }
}

impl JsHandle {
    pub fn object_id(&self) -> Option<&str> {
        self.remote_object.get("objectId").and_then(Value::as_str)
    }
    pub fn is_disposed(&self) -> bool {
        *self.disposed.lock().unwrap()
    }
    pub async fn dispose(&self) -> Result<()> {
        if std::mem::replace(&mut *self.disposed.lock().unwrap(), true) {
            return Ok(());
        }
        if let Some(object_id) = self.object_id() {
            self.page
                .send_session(
                    &self.session_id,
                    "Runtime.releaseObject",
                    json!({"objectId":object_id}),
                )
                .await?;
        }
        Ok(())
    }
    pub async fn json_value(&self) -> Result<JsValue> {
        validate_handle(self, self.context_id, &self.session_id)?;
        let Some(object) = self.object_id() else {
            return decode_primitive_remote(&self.remote_object);
        };
        let frame = Frame {
            page: self.page.clone(),
            id: self.frame_id.clone(),
        };
        let utility = frame
            .utility_object(self.context_id, &self.session_id)
            .await?;
        let wrapper = serde_json::to_string(EVALUATION_WRAPPER)
            .expect("the evaluation wrapper is always a JSON string");
        let declaration = format!(
            "(utilityScript, value) => utilityScript.evaluate(true, true, {wrapper}, 3, 'value => value', true, {{h: 0}}, value)"
        );
        let result = self.page.send_session(&self.session_id, "Runtime.callFunctionOn", json!({
            "functionDeclaration":declaration,"objectId":utility,
            "arguments":[{"objectId":utility},{"objectId":object}],"returnByValue":true,"awaitPromise":true
        })).await?;
        check_exception(&result)?;
        Ok(decode_serialized_remote(remote_object(&result)))
    }
    pub async fn evaluate_json(&self, expression: &str, arg: impl Into<CallArg>) -> Result<Value> {
        Ok(self.evaluate(expression, arg).await?.to_json())
    }
    pub async fn evaluate(&self, expression: &str, arg: impl Into<CallArg>) -> Result<JsValue> {
        validate_handle(self, self.context_id, &self.session_id)?;
        let frame = Frame {
            page: self.page.clone(),
            id: self.frame_id.clone(),
        };
        frame
            .evaluate_with_context(
                expression,
                vec![CallArg::Handle(self.clone()), arg.into()],
                true,
                self.context_id,
                &self.session_id,
            )
            .await
            .map(|v| v.0)
    }
    pub async fn evaluate_handle(
        &self,
        expression: &str,
        arg: impl Into<CallArg>,
    ) -> Result<JsHandle> {
        validate_handle(self, self.context_id, &self.session_id)?;
        let frame = Frame {
            page: self.page.clone(),
            id: self.frame_id.clone(),
        };
        let (_, remote) = frame
            .evaluate_with_context(
                expression,
                vec![CallArg::Handle(self.clone()), arg.into()],
                false,
                self.context_id,
                &self.session_id,
            )
            .await?;
        Ok(frame.make_handle(remote, self.session_id.clone(), self.context_id))
    }
    pub fn as_element(&self) -> Option<ElementHandle> {
        self.object_id().map(|_| ElementHandle(self.clone()))
    }
}

impl ElementHandle {
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let object = self
            .0
            .object_id()
            .ok_or_else(|| Error::InvalidArgument("ElementHandle has no object id".into()))?;
        self.0
            .page
            .send_session(
                &self.0.session_id,
                "DOM.scrollIntoViewIfNeeded",
                json!({"objectId":object}),
            )
            .await?;
        let quads = self
            .0
            .page
            .send_session(
                &self.0.session_id,
                "DOM.getContentQuads",
                json!({"objectId":object}),
            )
            .await?;
        let quad = quads
            .get("quads")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(Value::as_array)
            .ok_or_else(|| Error::InvalidArgument("Element is not visible".into()))?;
        let xs: Vec<f64> = quad.iter().step_by(2).filter_map(Value::as_f64).collect();
        let ys: Vec<f64> = quad
            .iter()
            .skip(1)
            .step_by(2)
            .filter_map(Value::as_f64)
            .collect();
        let x = xs.iter().copied().fold(f64::INFINITY, f64::min);
        let y = ys.iter().copied().fold(f64::INFINITY, f64::min);
        let width = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max) - x;
        let height = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max) - y;
        self.0
            .page
            .screenshot(ScreenshotOptions {
                clip: Some(crate::ScreenshotClip {
                    x,
                    y,
                    width,
                    height,
                }),
                ..Default::default()
            })
            .await
    }
}

impl Dialog {
    async fn handle(&self, accept: bool, prompt_text: Option<String>) -> Result<()> {
        if std::mem::replace(&mut *self.handled.lock().unwrap(), true) {
            return Ok(());
        }
        let mut params = json!({"accept":accept});
        if let Some(prompt_text) = prompt_text {
            params["promptText"] = json!(prompt_text);
        }
        let result = self
            .session
            .send("Page.handleJavaScriptDialog", params)
            .await;
        if result.is_err() {
            *self.handled.lock().unwrap() = false;
        }
        result.map(|_| ()).map_err(Into::into)
    }
    pub async fn accept(&self, prompt_text: Option<String>) -> Result<()> {
        self.handle(true, prompt_text).await
    }
    pub async fn dismiss(&self) -> Result<()> {
        self.handle(false, None).await
    }
}

impl FileChooser {
    pub async fn set_files(&self, paths: Vec<PathBuf>) -> Result<()> {
        let object = self.element.0.object_id().ok_or_else(|| {
            Error::InvalidArgument("File chooser element has no object id".into())
        })?;
        let files: Vec<String> = paths
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.element
            .0
            .page
            .send_session(
                &self.element.0.session_id,
                "DOM.setFileInputFiles",
                json!({"files":files,"objectId":object}),
            )
            .await?;
        Ok(())
    }
}

impl Download {
    pub async fn path(&self) -> Result<PathBuf> {
        loop {
            let (canceled, completed, file_path) = {
                let state = self.state.lock().unwrap();
                (state.canceled, state.completed, state.file_path.clone())
            };
            if canceled {
                return Err(Error::Navigation("Download canceled".into()));
            }
            if completed {
                return file_path
                    .or_else(|| {
                        self.download_path
                            .as_ref()
                            .map(|path| path.join(&self.guid))
                    })
                    .ok_or_else(|| Error::Navigation("Download path is not available".into()));
            }
            wait_change(&self.notify).await;
        }
    }
}

fn normalize_expression(expression: &str) -> String {
    let expression = expression.trim();
    if expression.starts_with("function") || expression.starts_with("async function") {
        format!("({expression})")
    } else {
        expression.to_owned()
    }
}

async fn wait_change(notify: &Notify) {
    tokio::select! {
        _ = notify.notified() => {},
        _ = tokio::time::sleep(Duration::from_millis(10)) => {},
    }
}

fn push_serialized(
    values: &mut Vec<Value>,
    handles: &mut Vec<Value>,
    arg: CallArg,
    context: u64,
    session: &str,
) -> Result<()> {
    match arg {
        CallArg::Value(value) => values.push(serialize_js_value(&value, &mut 0)),
        CallArg::Undefined => values.push(json!({"v":"undefined"})),
        CallArg::Handle(handle) => {
            validate_handle(&handle, context, session)?;
            values.push(json!({"h":handles.len()}));
            handles.push(handle_call_argument(&handle)?);
        }
    }
    Ok(())
}

fn serialize_js_value(value: &JsValue, last_id: &mut u64) -> Value {
    match value {
        JsValue::Null => json!({"v":"null"}),
        JsValue::Undefined => json!({"v":"undefined"}),
        JsValue::Bool(value) => json!(value),
        JsValue::Number(value) if value.is_nan() => json!({"v":"NaN"}),
        JsValue::Number(value) if *value == f64::INFINITY => json!({"v":"Infinity"}),
        JsValue::Number(value) if *value == f64::NEG_INFINITY => json!({"v":"-Infinity"}),
        JsValue::Number(value) if *value == 0.0 && value.is_sign_negative() => json!({"v":"-0"}),
        JsValue::Number(value) => json!(value),
        JsValue::BigInt(value) => json!({"bi":value}),
        JsValue::String(value) => json!(value),
        JsValue::Date(value) => json!({"d":value}),
        JsValue::RegExp { source, flags } => json!({"r":{"p":source,"f":flags}}),
        JsValue::Array(values) => {
            *last_id += 1;
            let id = *last_id;
            let values = values
                .iter()
                .map(|value| serialize_js_value(value, last_id))
                .collect::<Vec<_>>();
            json!({"a":values,"id":id})
        }
        JsValue::Object(entries) => {
            *last_id += 1;
            let id = *last_id;
            let entries = entries
                .iter()
                .map(|(key, value)| json!({"k":key,"v":serialize_js_value(value, last_id)}))
                .collect::<Vec<_>>();
            json!({"o":entries,"id":id})
        }
        JsValue::Map(entries) => {
            let entries = JsValue::Array(
                entries
                    .iter()
                    .map(|(key, value)| JsValue::Array(vec![key.clone(), value.clone()]))
                    .collect(),
            );
            serialize_collection("map", &entries, last_id)
        }
        JsValue::Set(values) => {
            serialize_collection("set", &JsValue::Array(values.clone()), last_id)
        }
    }
}

fn serialize_collection(kind: &str, value: &JsValue, last_id: &mut u64) -> Value {
    *last_id += 1;
    let id = *last_id;
    let entries = vec![
        json!({"k":COLLECTION_MARKER,"v":kind}),
        json!({"k":"value","v":serialize_js_value(value, last_id)}),
    ];
    json!({"o":entries,"id":id})
}

fn decode_serialized_remote(remote: &Value) -> JsValue {
    remote
        .get("value")
        .cloned()
        .map(decode_serialized)
        .unwrap_or(JsValue::Undefined)
}

fn decode_serialized(value: Value) -> JsValue {
    let Some(object) = value.as_object() else {
        return JsValue::from(value);
    };
    if let Some(v) = object.get("v").and_then(Value::as_str) {
        return match v {
            "null" => JsValue::Null,
            "undefined" => JsValue::Undefined,
            "NaN" => JsValue::Number(f64::NAN),
            "Infinity" => JsValue::Number(f64::INFINITY),
            "-Infinity" => JsValue::Number(f64::NEG_INFINITY),
            "-0" => JsValue::Number(-0.0),
            _ => JsValue::Undefined,
        };
    }
    if let Some(value) = object.get("bi").and_then(Value::as_str) {
        return JsValue::BigInt(value.to_owned());
    }
    if let Some(value) = object.get("d").and_then(Value::as_str) {
        return JsValue::Date(value.to_owned());
    }
    // JsValue intentionally exposes only the frozen public variants. Preserve
    // unsupported Playwright values in stable JSON-shaped forms instead of
    // silently dropping their payloads: URL -> string; Error, typed arrays,
    // ArrayBuffer, references, and function bindings -> named objects below.
    if let Some(value) = object.get("u").and_then(Value::as_str) {
        return JsValue::String(value.to_owned());
    }
    if let Some(regexp) = object.get("r") {
        return JsValue::RegExp {
            source: regexp
                .get("p")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            flags: regexp
                .get("f")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
    }
    if let Some(values) = object.get("a").and_then(Value::as_array) {
        return JsValue::Array(values.iter().cloned().map(decode_serialized).collect());
    }
    if let Some(entries) = object.get("o").and_then(Value::as_array) {
        let mut result = Vec::new();
        for entry in entries {
            if let (Some(k), Some(v)) = (entry.get("k").and_then(Value::as_str), entry.get("v")) {
                result.push((k.to_owned(), decode_serialized(v.clone())));
            }
        }
        return decode_collection(result);
    }
    if let Some(error) = object.get("e") {
        return JsValue::Object(vec![
            (
                "name".into(),
                JsValue::String(
                    error
                        .get("n")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            ),
            (
                "message".into(),
                JsValue::String(
                    error
                        .get("m")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            ),
            (
                "stack".into(),
                JsValue::String(
                    error
                        .get("s")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            ),
        ]);
    }
    if let Some(typed_array) = object.get("ta") {
        return JsValue::Object(vec![
            (
                "type".into(),
                JsValue::String(
                    typed_array
                        .get("k")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            ),
            (
                "base64".into(),
                JsValue::String(
                    typed_array
                        .get("b")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ),
            ),
        ]);
    }
    if let Some(array_buffer) = object.get("ab") {
        return JsValue::Object(vec![(
            "base64".into(),
            JsValue::String(
                array_buffer
                    .get("b")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
        )]);
    }
    if let Some(reference) = object.get("ref").and_then(Value::as_u64) {
        return JsValue::Object(vec![("$ref".into(), JsValue::Number(reference as f64))]);
    }
    if let Some(function) = object.get("fn").and_then(Value::as_str) {
        return JsValue::Object(vec![(
            "$functionBinding".into(),
            JsValue::String(function.to_owned()),
        )]);
    }
    JsValue::from(value)
}

fn decode_collection(entries: Vec<(String, JsValue)>) -> JsValue {
    if entries.len() == 2 {
        let kind = entries.iter().find_map(|(key, value)| {
            (key == COLLECTION_MARKER).then_some(match value {
                JsValue::String(value) => Some(value.as_str()),
                _ => None,
            })?
        });
        let value = entries
            .iter()
            .find_map(|(key, value)| (key == "value").then_some(value));
        if let (Some(kind), Some(JsValue::Array(values))) = (kind, value) {
            if kind == "set" {
                return JsValue::Set(values.clone());
            }
            if kind == "map" {
                let mut map = Vec::with_capacity(values.len());
                for pair in values {
                    let JsValue::Array(pair) = pair else {
                        return JsValue::Object(entries);
                    };
                    if pair.len() != 2 {
                        return JsValue::Object(entries);
                    }
                    map.push((pair[0].clone(), pair[1].clone()));
                }
                return JsValue::Map(map);
            }
        }
    }
    JsValue::Object(entries)
}

fn decode_primitive_remote(remote: &Value) -> Result<JsValue> {
    if remote.get("type").and_then(Value::as_str) == Some("undefined") {
        return Ok(JsValue::Undefined);
    }
    if let Some(value) = remote.get("unserializableValue").and_then(Value::as_str) {
        return Ok(match value {
            "NaN" => JsValue::Number(f64::NAN),
            "Infinity" => JsValue::Number(f64::INFINITY),
            "-Infinity" => JsValue::Number(f64::NEG_INFINITY),
            "-0" => JsValue::Number(-0.0),
            bigint if bigint.ends_with('n') => {
                JsValue::BigInt(bigint[..bigint.len() - 1].to_owned())
            }
            value => JsValue::String(value.to_owned()),
        });
    }
    if let Some(value) = remote.get("value") {
        return Ok(JsValue::from(value.clone()));
    }
    Err(Error::Evaluation(
        "JSHandle does not contain a serializable primitive".into(),
    ))
}

fn handle_call_argument(handle: &JsHandle) -> Result<Value> {
    if let Some(object_id) = handle.object_id() {
        return Ok(json!({"objectId":object_id}));
    }
    if let Some(value) = handle.remote_object.get("unserializableValue") {
        return Ok(json!({"unserializableValue":value}));
    }
    if handle.remote_object.get("type").and_then(Value::as_str) == Some("undefined") {
        return Ok(json!({}));
    }
    if let Some(value) = handle.remote_object.get("value") {
        return Ok(json!({"value":value}));
    }
    Err(Error::Evaluation(
        "JSHandle cannot be passed as an argument".into(),
    ))
}

fn validate_handle(handle: &JsHandle, context: u64, session: &str) -> Result<()> {
    if handle.is_disposed() {
        return Err(Error::Evaluation("JSHandle is disposed!".into()));
    }
    if handle.context_id != context || handle.session_id != session {
        return Err(Error::Evaluation(
            "JSHandles can be evaluated only in the context they were created!".into(),
        ));
    }
    Ok(())
}

fn remote_object(result: &Value) -> &Value {
    result.get("result").unwrap_or(&Value::Null)
}
fn remote_value(remote: &Value) -> Value {
    if let Some(value) = remote.get("value") {
        return value.clone();
    }
    match remote.get("unserializableValue").and_then(Value::as_str) {
        Some("NaN" | "Infinity" | "-Infinity" | "-0") => Value::Null,
        Some(value) => Value::String(value.to_owned()),
        None => Value::Null,
    }
}
fn check_exception(result: &Value) -> Result<()> {
    let Some(details) = result.get("exceptionDetails") else {
        return Ok(());
    };
    let message = details
        .get("exception")
        .and_then(|v| v.get("description").or_else(|| v.get("value")))
        .and_then(Value::as_str)
        .or_else(|| details.get("text").and_then(Value::as_str))
        .unwrap_or("Evaluation failed");
    Err(Error::Evaluation(
        message
            .strip_prefix("Error: ")
            .unwrap_or(message)
            .to_owned(),
    ))
}

fn is_user_page_target_info(info: &Value) -> bool {
    info.get("type").and_then(Value::as_str) == Some("page")
        && !info
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .starts_with("chrome-extension://")
}

fn is_user_page_target(target: &TargetData) -> bool {
    target.type_ == "page" && !target.url.starts_with("chrome-extension://")
}

fn normalize_target_context_id(
    browser: &BrowserInner,
    type_: &str,
    url: &str,
    reported_context_id: Option<String>,
) -> Option<String> {
    let mut default_cdp_context_id = browser.default_cdp_context_id.write().unwrap();
    if let Some(default_context_id) = default_cdp_context_id.as_ref() {
        return if reported_context_id.as_deref() == default_context_id.as_deref() {
            None
        } else {
            reported_context_id
        };
    }
    if type_ == "page" && !url.starts_with("chrome-extension://") {
        *default_cdp_context_id = Some(reported_context_id);
        None
    } else {
        reported_context_id
    }
}

fn update_target_info(browser: &Arc<BrowserInner>, info: &Value) -> Option<String> {
    let target_id = info.get("targetId")?.as_str()?.to_owned();
    let type_ = info
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let url = info
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let user_page = type_ == "page" && !url.starts_with("chrome-extension://");
    let context_id = normalize_target_context_id(
        browser,
        &type_,
        &url,
        info.get("browserContextId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    let attached = info.get("attached").and_then(Value::as_bool);

    {
        let mut targets = browser.targets.write().unwrap();
        let target = targets
            .entry(target_id.clone())
            .or_insert_with(|| TargetData {
                sequence: browser.next_target_sequence.fetch_add(1, Ordering::SeqCst),
                type_: String::new(),
                url: String::new(),
                context_id: None,
                session_id: None,
                attached: false,
                initialization: PageInitialization::Pending,
                reported: false,
                page: None,
                opener_id: None,
            });
        target.type_ = type_;
        target.url = url;
        target.context_id = context_id.clone();
        target.opener_id = info
            .get("openerId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(attached) = attached {
            target.attached = attached;
            if !attached {
                target.initialization = PageInitialization::Pending;
            }
        }
    }
    if user_page
        && context_id.is_some()
        && !browser.contexts.read().unwrap().contains_key(&context_id)
    {
        browser.contexts.write().unwrap().insert(
            context_id.clone(),
            BrowserContext {
                browser: Arc::downgrade(browser),
                id: context_id,
            },
        );
    }
    browser.notify.notify_waiters();
    Some(target_id)
}

fn handle_target_destroyed(browser: &Arc<BrowserInner>, target_id: &str) {
    let target = browser.targets.write().unwrap().remove(target_id);
    browser
        .closed_targets
        .lock()
        .unwrap()
        .insert(target_id.to_owned());
    if let Some(session_id) = target
        .as_ref()
        .and_then(|target| target.session_id.as_deref())
    {
        browser.sessions.write().unwrap().remove(session_id);
        browser.session_targets.write().unwrap().remove(session_id);
    }
    let page = target
        .and_then(|target| target.page)
        .or_else(|| browser.pages.write().unwrap().remove(target_id));
    browser.pages.write().unwrap().remove(target_id);
    browser
        .sessions
        .write()
        .unwrap()
        .retain(|_, id| id != target_id);
    browser
        .session_targets
        .write()
        .unwrap()
        .retain(|_, id| id != target_id);
    if let Some(page) = page {
        page.inner.state.lock().unwrap().closed = true;
        page.inner.notify.notify_waiters();
    }
    browser.notify.notify_waiters();
}

fn handle_target_detached(browser: &Arc<BrowserInner>, params: &Value) {
    let session_id = params.get("sessionId").and_then(Value::as_str);
    let mapped_target_id =
        session_id.and_then(|session| browser.sessions.write().unwrap().remove(session));
    let attached_target_id =
        session_id.and_then(|session| browser.session_targets.write().unwrap().remove(session));
    let target_id = params
        .get("targetId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(attached_target_id);
    if let Some(target_id) = target_id {
        let (type_, page) =
            if let Some(target) = browser.targets.write().unwrap().get_mut(&target_id) {
                target.session_id = None;
                target.attached = false;
                target.initialization = PageInitialization::Pending;
                (target.type_.clone(), target.page.take())
            } else {
                (String::new(), None)
            };
        if type_ == "page" {
            browser
                .closed_targets
                .lock()
                .unwrap()
                .insert(target_id.clone());
            let page = page.or_else(|| browser.pages.write().unwrap().remove(&target_id));
            browser.pages.write().unwrap().remove(&target_id);
            if let Some(page) = page {
                page.inner.state.lock().unwrap().closed = true;
                page.inner.notify.notify_waiters();
            }
        } else if type_ == "iframe" {
            if let (Some(session_id), Some(parent_target_id)) =
                (session_id, mapped_target_id.as_deref())
            {
                if let Some(page) = browser.pages.read().unwrap().get(parent_target_id).cloned() {
                    let mut state = page.inner.state.lock().unwrap();
                    for frame in state
                        .frames
                        .values_mut()
                        .filter(|frame| frame.owner_session == session_id)
                    {
                        frame.detached = true;
                        frame.contexts.clear();
                    }
                    drop(state);
                    page.inner.notify.notify_waiters();
                }
            }
        }
        browser.notify.notify_waiters();
    }
}

fn handle_event(browser: &Arc<BrowserInner>, event: Event) {
    match event.method.as_str() {
        "Target.targetCreated" | "Target.targetInfoChanged" => {
            if let Some(info) = event.params.get("targetInfo") {
                update_target_info(browser, info);
            }
        }
        "Target.attachedToTarget" => handle_attached(browser, &event),
        "Target.targetDestroyed" => {
            if let Some(id) = event.params.get("targetId").and_then(Value::as_str) {
                handle_target_destroyed(browser, id);
            }
        }
        "Target.detachedFromTarget" => handle_target_detached(browser, &event.params),
        "Browser.downloadWillBegin" => handle_download_begin(browser, &event.params),
        "Browser.downloadProgress" => handle_download_progress(browser, &event.params),
        _ => {
            let Some(session_id) = event.session_id.as_deref() else {
                return;
            };
            let target = browser.sessions.read().unwrap().get(session_id).cloned();
            let page = target.and_then(|id| browser.pages.read().unwrap().get(&id).cloned());
            if let Some(page) = page {
                handle_page_event(&page, session_id, &event.method, &event.params);
            }
        }
    }
}

fn handle_attached(browser: &Arc<BrowserInner>, event: &Event) {
    let Some(session_id) = event
        .params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let Some(info) = event.params.get("targetInfo") else {
        return;
    };
    let Some(target_id) = update_target_info(browser, info) else {
        return;
    };
    browser.closed_targets.lock().unwrap().remove(&target_id);
    browser
        .session_targets
        .write()
        .unwrap()
        .insert(session_id.clone(), target_id.clone());
    let type_ = info.get("type").and_then(Value::as_str).unwrap_or_default();
    if let Some(target) = browser.targets.write().unwrap().get_mut(&target_id) {
        target.session_id = Some(session_id.clone());
        target.attached = true;
        target.initialization = PageInitialization::Pending;
    }
    if type_ == "page" {
        let context_id = browser
            .targets
            .read()
            .unwrap()
            .get(&target_id)
            .and_then(|target| target.context_id.clone());
        let page = Page::new(browser, target_id.clone(), context_id, session_id.clone());
        let replaced_page = browser
            .pages
            .write()
            .unwrap()
            .insert(target_id.clone(), page.clone());
        if let Some(replaced_page) = replaced_page {
            replaced_page.inner.state.lock().unwrap().closed = true;
            replaced_page.inner.notify.notify_waiters();
        }
        browser
            .sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), target_id);
        if let Some(target) = browser
            .targets
            .write()
            .unwrap()
            .get_mut(&page.inner.target_id)
        {
            target.page = Some(page.clone());
        }
        browser.notify.notify_waiters();
        tokio::spawn(async move {
            let _ = initialize_session(page, session_id, true).await;
        });
    } else if type_ == "iframe" {
        let parent_target = event
            .session_id
            .as_ref()
            .and_then(|id| browser.sessions.read().unwrap().get(id).cloned());
        let Some(parent_target) = parent_target else {
            return;
        };
        let Some(page) = browser.pages.read().unwrap().get(&parent_target).cloned() else {
            return;
        };
        browser
            .sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), parent_target);
        let parent = info
            .get("parentFrameId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        ensure_frame(&page, &target_id, parent, &session_id, false);
        tokio::spawn(async move {
            let _ = initialize_session(page, session_id, false).await;
        });
    } else {
        let connection = browser.connection.clone();
        tokio::spawn(async move {
            let _ = connection
                .send(
                    "Runtime.runIfWaitingForDebugger",
                    json!({}),
                    Some(&session_id),
                )
                .await;
        });
    }
}

fn finish_page_initialization(page: &Page, result: &Result<()>) {
    let Some(browser) = page.inner.browser.upgrade() else {
        return;
    };
    let mut handlers = Vec::new();
    let mut ready = false;
    let mut closed = false;

    match result {
        Ok(()) => {
            let mut targets = browser.targets.write().unwrap();
            if let Some(target) = targets.get_mut(&page.inner.target_id) {
                if target.attached {
                    target.initialization = PageInitialization::Ready;
                    ready = true;
                    if is_user_page_target(target) && !target.reported {
                        target.reported = true;
                        let sequence = target.sequence;
                        handlers = browser
                            .target_handlers
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|listener| sequence >= listener.first_sequence)
                            .map(|listener| listener.handler.clone())
                            .collect();
                    }
                } else {
                    closed = true;
                }
            } else {
                closed = true;
            }
        }
        Err(error) => {
            let reason = format!("Failed to initialize page: {error}");
            if let Some(target) = browser
                .targets
                .write()
                .unwrap()
                .get_mut(&page.inner.target_id)
            {
                target.initialization = PageInitialization::Unusable {
                    reason: reason.clone(),
                };
            }
            page.inner.state.lock().unwrap().initialization_error = Some(reason.clone());
            tracing::warn!(
                target_id = %page.inner.target_id,
                error = %reason,
                "page initialization failed; target will remain unusable until retried"
            );
        }
    }

    if ready || closed {
        let mut state = page.inner.state.lock().unwrap();
        state.ready = ready;
        state.closed = closed;
    }
    page.inner.notify.notify_waiters();
    browser.notify.notify_waiters();
    if ready {
        for handler in handlers {
            handler(page.clone());
        }
    }
}

async fn initialize_session(page: Page, session_id: String, main: bool) -> Result<()> {
    let send = |method: &'static str, params: Value| {
        let page = page.clone();
        let session = session_id.clone();
        async move { page.send_session(&session, method, params).await }
    };
    let setup_result = async {
        send("Page.enable", json!({})).await?;
        let tree = send("Page.getFrameTree", json!({})).await?;
        if let Some(tree) = tree.get("frameTree") {
            seed_frame_tree(&page, tree, &session_id, main);
        }
        send("Runtime.enable", json!({})).await?;
        send("Network.enable", json!({})).await?;
        send("Page.setLifecycleEventsEnabled", json!({"enabled":true})).await?;
        send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source":"","worldName":UTILITY_WORLD}),
        )
        .await?;
        send(
            "Page.setInterceptFileChooserDialog",
            json!({"enabled":true}),
        )
        .await?;
        send(
            "Target.setAutoAttach",
            json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}),
        )
        .await?;
        let frame_ids: Vec<String> = {
            let state = page.inner.state.lock().unwrap();
            state
                .frames
                .iter()
                .filter(|(_, frame)| frame.owner_session == session_id)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for frame_id in frame_ids {
            let _ = send(
                "Page.createIsolatedWorld",
                json!({"frameId":frame_id,"worldName":UTILITY_WORLD,"grantUniveralAccess":true}),
            )
            .await;
        }
        Ok(())
    }
    .await;
    let resume_result = send("Runtime.runIfWaitingForDebugger", json!({}))
        .await
        .map(|_| ());
    let result = setup_result.and(resume_result);
    if main {
        finish_page_initialization(&page, &result);
    }
    result
}

fn seed_frame_tree(page: &Page, tree: &Value, session: &str, main: bool) {
    let Some(frame) = tree.get("frame") else {
        return;
    };
    let Some(id) = frame.get("id").and_then(Value::as_str) else {
        return;
    };
    let parent = frame
        .get("parentId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    ensure_frame(page, id, parent, session, main);
    update_frame_navigation(page, frame, session, false);
    if let Some(children) = tree.get("childFrames").and_then(Value::as_array) {
        for child in children {
            seed_frame_tree(page, child, session, false);
        }
    }
}

fn ensure_frame(page: &Page, id: &str, parent: Option<String>, session: &str, main: bool) {
    let mut state = page.inner.state.lock().unwrap();
    if !state.frames.contains_key(id) {
        let seq = state.next_seq;
        state.next_seq += 1;
        state.frames.insert(
            id.to_owned(),
            FrameData {
                parent,
                url: String::new(),
                name: String::new(),
                seq,
                detached: false,
                owner_session: session.to_owned(),
                contexts: HashMap::new(),
                lifecycle: HashSet::from([LoadState::Commit]),
                loader_id: None,
            },
        );
    } else if let Some(frame) = state.frames.get_mut(id) {
        frame.detached = false;
        frame.owner_session = session.to_owned();
    }
    if main || state.main_frame.is_none() {
        state.main_frame = Some(id.to_owned());
    }
    drop(state);
    page.inner.notify.notify_waiters();
}

fn update_frame_navigation(
    page: &Page,
    payload: &Value,
    session: &str,
    restored_from_bfcache: bool,
) {
    let Some(id) = payload.get("id").and_then(Value::as_str) else {
        return;
    };
    ensure_frame(
        page,
        id,
        payload
            .get("parentId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        session,
        false,
    );
    let mut state = page.inner.state.lock().unwrap();
    let descendants = descendant_ids(&state.frames, id);
    for child in descendants {
        if child != id {
            if let Some(frame) = state.frames.get_mut(&child) {
                frame.detached = true;
            }
        }
    }
    if let Some(frame) = state.frames.get_mut(id) {
        frame.url = format!(
            "{}{}",
            payload
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            payload
                .get("urlFragment")
                .and_then(Value::as_str)
                .unwrap_or_default()
        );
        frame.name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        frame.loader_id = payload
            .get("loaderId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        frame.lifecycle.clear();
        frame.lifecycle.insert(LoadState::Commit);
        // Chromium does not re-emit these events when restoring an already-loaded document.
        if restored_from_bfcache {
            frame.lifecycle.insert(LoadState::DomContentLoaded);
            frame.lifecycle.insert(LoadState::Load);
        }
        if !restored_from_bfcache {
            frame.contexts.clear();
        }
    }
    if state.main_frame.as_deref() == Some(id) {
        state.navigation_generation += 1;
    }
    drop(state);
    page.inner.notify.notify_waiters();
}

fn descendant_ids(frames: &HashMap<String, FrameData>, root: &str) -> Vec<String> {
    let mut result = vec![root.to_owned()];
    let mut index = 0;
    while index < result.len() {
        let parent = result[index].clone();
        result.extend(
            frames
                .iter()
                .filter(|(_, f)| f.parent.as_deref() == Some(&parent))
                .map(|(id, _)| id.clone()),
        );
        index += 1;
    }
    result
}

fn handle_page_event(page: &Page, session: &str, method: &str, params: &Value) {
    match method {
        "Page.frameAttached" => {
            if let Some(id) = params.get("frameId").and_then(Value::as_str) {
                ensure_frame(
                    page,
                    id,
                    params
                        .get("parentFrameId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    session,
                    false,
                );
            }
        }
        "Page.frameNavigated" => {
            if let Some(frame) = params.get("frame") {
                update_frame_navigation(
                    page,
                    frame,
                    session,
                    params.get("type").and_then(Value::as_str) == Some("BackForwardCacheRestore"),
                );
            }
        }
        "Page.navigatedWithinDocument" => {
            if let (Some(id), Some(url)) = (
                params.get("frameId").and_then(Value::as_str),
                params.get("url").and_then(Value::as_str),
            ) {
                let mut state = page.inner.state.lock().unwrap();
                if let Some(frame) = state.frames.get_mut(id) {
                    frame.url = url.to_owned();
                }
                if state.main_frame.as_deref() == Some(id) {
                    state.navigation_generation += 1;
                }
                drop(state);
                page.inner.notify.notify_waiters();
            }
        }
        "Page.frameDetached" => {
            if params.get("reason").and_then(Value::as_str) != Some("swap") {
                if let Some(id) = params.get("frameId").and_then(Value::as_str) {
                    let mut state = page.inner.state.lock().unwrap();
                    for child in descendant_ids(&state.frames, id) {
                        if let Some(frame) = state.frames.get_mut(&child) {
                            frame.detached = true;
                            frame.contexts.clear();
                        }
                    }
                    drop(state);
                    page.inner.notify.notify_waiters();
                }
            }
        }
        "Page.lifecycleEvent" => {
            if let (Some(id), Some(name)) = (
                params.get("frameId").and_then(Value::as_str),
                params.get("name").and_then(Value::as_str),
            ) {
                let lifecycle = match name {
                    "load" => Some(LoadState::Load),
                    "DOMContentLoaded" => Some(LoadState::DomContentLoaded),
                    _ => None,
                };
                if let Some(lifecycle) = lifecycle {
                    if let Some(frame) = page.inner.state.lock().unwrap().frames.get_mut(id) {
                        frame.lifecycle.insert(lifecycle);
                    }
                    page.inner.notify.notify_waiters();
                }
            }
        }
        "Runtime.executionContextCreated" => handle_context_created(page, session, params),
        "Runtime.executionContextDestroyed" => {
            if let Some(id) = params.get("executionContextId").and_then(Value::as_u64) {
                destroy_context(page, session, Some(id));
            }
        }
        "Runtime.executionContextsCleared" => destroy_context(page, session, None),
        "Network.requestWillBeSent" => handle_request(page, params),
        "Network.responseReceived" => handle_response(page, params),
        "Network.loadingFinished" => finish_request(page, params, None),
        "Network.loadingFailed" => finish_request(
            page,
            params,
            params.get("errorText").and_then(Value::as_str),
        ),
        "Page.javascriptDialogOpening" => handle_dialog(page, session, params),
        "Runtime.consoleAPICalled" => handle_console(page, session, params),
        "Runtime.exceptionThrown" => {
            let message = params
                .get("exceptionDetails")
                .and_then(|d| {
                    d.get("exception")
                        .and_then(|e| e.get("description"))
                        .or_else(|| d.get("text"))
                })
                .and_then(Value::as_str)
                .unwrap_or("Uncaught exception")
                .to_owned();
            for handler in page.inner.page_error_handlers.lock().unwrap().clone() {
                handler(PageError {
                    message: message.clone(),
                });
            }
        }
        "Page.fileChooserOpened" => handle_file_chooser(page, session, params),
        _ => {}
    }
}

fn handle_context_created(page: &Page, session: &str, params: &Value) {
    let Some(context) = params.get("context") else {
        return;
    };
    let Some(id) = context.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(frame_id) = context
        .get("auxData")
        .and_then(|v| v.get("frameId"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let world = if context
        .get("auxData")
        .and_then(|v| v.get("isDefault"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        Some(World::Main)
    } else if context.get("name").and_then(Value::as_str) == Some(UTILITY_WORLD) {
        Some(World::Utility)
    } else {
        None
    };
    let Some(world) = world else { return };
    let mut state = page.inner.state.lock().unwrap();
    if let Some(frame) = state.frames.get_mut(frame_id) {
        if frame.owner_session == session {
            frame.contexts.insert(
                world,
                ContextData {
                    id,
                    ..Default::default()
                },
            );
        }
    }
    drop(state);
    page.inner.notify.notify_waiters();
}

fn destroy_context(page: &Page, session: &str, context_id: Option<u64>) {
    let mut state = page.inner.state.lock().unwrap();
    for frame in state
        .frames
        .values_mut()
        .filter(|f| f.owner_session == session)
    {
        frame
            .contexts
            .retain(|_, context| context_id.is_some_and(|id| context.id != id));
    }
    drop(state);
    page.inner.notify.notify_waiters();
}

fn handle_request(page: &Page, params: &Value) {
    let Some(request_id) = params
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let loader_id = params
        .get("loaderId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let resource_type = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let request = params.get("request");
    let mut state = page.inner.state.lock().unwrap();
    if let Some(request) = request {
        state.network_sequence += 1;
        let sequence = state.network_sequence;
        let headers = request
            .get("headers")
            .and_then(Value::as_object)
            .map(|headers| {
                headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.clone(),
                            value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        state.request_events.push((
            sequence,
            Request {
                url: request
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                method: request
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                headers,
                post_data: request
                    .get("postData")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        ));
        if state.request_events.len() > 512 {
            state.request_events.drain(..256);
        }
    }
    if resource_type == "Document" {
        state.loader_requests.insert(loader_id, request_id.clone());
    }
    if resource_type != "WebSocket" && resource_type != "EventSource" {
        state.inflight.insert(request_id);
        state.last_network_activity = Instant::now();
    }
    drop(state);
    page.inner.notify.notify_waiters();
}

fn handle_response(page: &Page, params: &Value) {
    let Some(request_id) = params
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let Some(response) = params.get("response") else {
        return;
    };
    let status = response
        .get("status")
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as u16;
    let headers = response
        .get("headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let response = Response {
        url: response
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        status,
        ok: (200..400).contains(&status),
        headers,
    };
    let mut state = page.inner.state.lock().unwrap();
    state.responses.insert(request_id, response.clone());
    state.network_sequence += 1;
    let sequence = state.network_sequence;
    state.response_events.push((sequence, response));
    if state.response_events.len() > 512 {
        state.response_events.drain(..256);
    }
    drop(state);
    page.inner.notify.notify_waiters();
}

fn finish_request(page: &Page, params: &Value, failure: Option<&str>) {
    let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
        return;
    };
    let mut state = page.inner.state.lock().unwrap();
    state.inflight.remove(request_id);
    state.last_network_activity = Instant::now();
    if let Some(failure) = failure {
        state
            .failures
            .insert(request_id.to_owned(), failure.to_owned());
    }
    drop(state);
    page.inner.notify.notify_waiters();
}

fn handle_dialog(page: &Page, session: &str, params: &Value) {
    let type_ = match params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("alert")
    {
        "beforeunload" => DialogType::BeforeUnload,
        "confirm" => DialogType::Confirm,
        "prompt" => DialogType::Prompt,
        _ => DialogType::Alert,
    };
    let Some(browser) = page.inner.browser.upgrade() else {
        return;
    };
    let dialog = Dialog {
        session: browser.connection.session(session.to_owned()),
        type_,
        message: params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        default_value: params
            .get("defaultPrompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        handled: Arc::new(Mutex::new(false)),
    };
    let handlers = page.inner.dialog_handlers.lock().unwrap().clone();
    if handlers.is_empty() {
        tokio::spawn(async move {
            let _ = dialog.dismiss().await;
        });
    } else {
        for handler in handlers {
            handler(dialog.clone());
        }
    }
}

fn handle_console(page: &Page, session: &str, params: &Value) {
    let context_id = params
        .get("executionContextId")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let frame_id = {
        let state = page.inner.state.lock().unwrap();
        state
            .frames
            .iter()
            .find(|(_, f)| {
                f.owner_session == session && f.contexts.values().any(|c| c.id == context_id)
            })
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| page.inner.target_id.clone())
    };
    let mut text = Vec::new();
    let mut args = Vec::new();
    for remote in params
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        text.push(remote_text(remote));
        args.push(JsHandle {
            page: page.clone(),
            remote_object: remote.clone(),
            session_id: session.to_owned(),
            context_id,
            frame_id: frame_id.clone(),
            disposed: Arc::new(Mutex::new(false)),
        });
    }
    let message = ConsoleMessage {
        type_: params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        text: text.join(" "),
        args,
    };
    for handler in page.inner.console_handlers.lock().unwrap().clone() {
        handler(message.clone());
    }
}

fn remote_text(remote: &Value) -> String {
    if let Some(value) = remote.get("value") {
        return match value {
            Value::String(v) => v.clone(),
            _ => value.to_string(),
        };
    }
    remote
        .get("unserializableValue")
        .or_else(|| remote.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn handle_file_chooser(page: &Page, session: &str, params: &Value) {
    let Some(backend_id) = params.get("backendNodeId").and_then(Value::as_u64) else {
        return;
    };
    let handlers = page.inner.file_chooser_handlers.lock().unwrap().clone();
    if handlers.is_empty() {
        return;
    }
    let page = page.clone();
    let session = session.to_owned();
    let is_multiple = params.get("mode").and_then(Value::as_str) == Some("selectMultiple");
    let frame_id = params
        .get("frameId")
        .and_then(Value::as_str)
        .unwrap_or(&page.inner.target_id)
        .to_owned();
    tokio::spawn(async move {
        let context = Frame {
            page: page.clone(),
            id: frame_id.clone(),
        }
        .context_with_timeout(World::Utility, None)
        .await
        .ok();
        let Some(context) = context else { return };
        let resolved = page
            .send_session(
                &session,
                "DOM.resolveNode",
                json!({"backendNodeId":backend_id,"executionContextId":context}),
            )
            .await
            .ok();
        let object = resolved
            .as_ref()
            .and_then(|v| v.get("object"))
            .and_then(|v| v.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(object) = object else { return };
        let chooser = FileChooser {
            element: ElementHandle(JsHandle {
                page: page.clone(),
                remote_object: json!({"objectId": object}),
                session_id: session,
                context_id: context,
                frame_id,
                disposed: Arc::new(Mutex::new(false)),
            }),
            is_multiple,
        };
        for handler in handlers {
            handler(chooser.clone());
        }
    });
}

fn handle_download_begin(browser: &Arc<BrowserInner>, params: &Value) {
    let Some(frame_id) = params.get("frameId").and_then(Value::as_str) else {
        return;
    };
    let guid = params
        .get("guid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let page = browser
        .targets
        .read()
        .unwrap()
        .values()
        .filter(|target| {
            target.attached && matches!(target.initialization, PageInitialization::Ready)
        })
        .filter_map(|target| target.page.as_ref())
        .find(|page| {
            page.inner
                .state
                .lock()
                .unwrap()
                .frames
                .get(frame_id)
                .is_some_and(|frame| !frame.detached)
        })
        .cloned();
    let context = page.as_ref().and_then(|page| page.inner.context_id.clone());
    let path = browser
        .download_paths
        .lock()
        .unwrap()
        .get(&context)
        .cloned();
    let download = Download {
        url: params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        suggested_filename: params
            .get("suggestedFilename")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        guid: guid.clone(),
        download_path: path,
        state: Arc::new(Mutex::new(DownloadState::default())),
        notify: Arc::new(Notify::new()),
    };
    browser
        .downloads
        .lock()
        .unwrap()
        .insert(guid, download.clone());
    if let Some(page) = page {
        let handlers = page.inner.download_handlers.lock().unwrap().clone();
        for handler in handlers {
            handler(download.clone());
        }
    }
    for handler in browser.download_handlers.lock().unwrap().clone() {
        handler(download.clone());
    }
}
fn handle_download_progress(browser: &Arc<BrowserInner>, params: &Value) {
    let Some(guid) = params.get("guid").and_then(Value::as_str) else {
        return;
    };
    let download = browser.downloads.lock().unwrap().get(guid).cloned();
    let Some(download) = download else { return };
    let mut state = download.state.lock().unwrap();
    if let Some(file_path) = params.get("filePath").and_then(Value::as_str) {
        state.file_path = Some(PathBuf::from(file_path));
    }
    match params.get("state").and_then(Value::as_str) {
        Some("completed") => state.completed = true,
        Some("canceled") => state.canceled = true,
        _ => {}
    }
    let finished = state.completed || state.canceled;
    drop(state);
    download.notify.notify_waiters();
    if finished {
        browser.downloads.lock().unwrap().remove(guid);
    }
}
