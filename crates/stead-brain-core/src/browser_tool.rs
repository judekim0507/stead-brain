use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
    ToolExecutionMode,
};
use rquickjs::function::{Async, Func};
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Promise, async_with};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use stead_brain_protocol::{BrainEvent, ResponseEnvelope, TabContext, ToolStatus};
use steadwright::{
    ActionOptions, AriaSnapshotOptions, Browser, BrowserContext, ByRoleOptions, CallArg, Dialog,
    DialogType, Download, ElementHandle, FileChooser, Frame, FrameLocator, GotoOptions, JsHandle,
    Keyboard, LoadState, Locator, LocatorFilter, Mouse, MouseButton, Page, PageEvent, Point,
    Polling, Response, ScreenshotFormat, ScreenshotOptions, SelectOptionValue, TextMatch,
    UrlMatcher, WaitForFunctionOptions, WaitForOptions, WaitForSelectorState, WaitForUrlOptions,
};
use steadwright_cdp::transport::Incoming;
use steadwright_cdp::{
    CdpError, Connection, FdPairTransport, Transport, TransportError, WebSocketTransport,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::BrowserToolBridge;

const TOOL_DESCRIPTION: &str = "Run Playwright JavaScript against the user's browser. Globals: page (current tab), context, browser, state (persists across calls), console. Top-level await and return are supported.";
const MAX_CODE_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 32 * 1024;
const MAX_LOG_BYTES: usize = 16 * 1024;
const MAX_IMAGES: usize = 4;
const MEMORY_LIMIT: usize = 32 * 1024 * 1024;
const STACK_LIMIT: usize = 1024 * 1024;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(120);
const RAW_CDP_ID_BASE: u64 = 8_000_000_000_000_000;
const UNAVAILABLE: &str = "Browser control is unavailable: stead-brain was not launched by Stead.";
const PHASE_6_REQUIRED: &str = "Credential fill requires the Phase 6 browser build.";

type RawResponse = oneshot::Sender<Result<Value, String>>;
type RawPending = Arc<StdMutex<HashMap<u64, RawResponse>>>;

const BOOTSTRAP: &str = r#"
(() => {
  const proxies = new Map();
  const syncMethods = new Set([
    'locator', 'frameLocator', 'getByRole', 'getByText', 'getByLabel',
    'getByPlaceholder', 'getByAltText', 'getByTitle', 'getByTestId',
    'first', 'last', 'nth', 'filter', 'and', 'or', 'contentFrame', 'mainFrame',
    'contexts', 'pages', 'url', 'frames', 'viewportSize', 'isClosed',
    'setDefaultTimeout', 'setDefaultNavigationTimeout', 'name', 'isDetached',
    'parentFrame', 'childFrames', 'status', 'ok', 'headers', 'asElement',
    'isMultiple', 'element', 'suggestedFilename', 'type', 'message', 'defaultValue'
  ]);
  const encode = value => JSON.stringify(value, (_key, item) => {
    if (item instanceof RegExp) return {__sw_regex: item.source, __sw_flags: item.flags};
    if (typeof item === 'function') return {__sw_function: item.toString()};
    if (item && item.__steadwrightRef !== undefined)
      return {__sw_ref_arg: item.__steadwrightRef};
    if (item && item.__sw_ref !== undefined)
      return {__sw_ref_arg: item.__sw_ref};
    return item;
  });
  const decode = value => {
    if (!value || typeof value !== 'object') return value;
    if (value.__sw_ref !== undefined) return makeProxy(value.__sw_ref, value.__sw_type);
    if (value.__sw_bytes !== undefined) {
      const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
      const source = value.__sw_bytes.replace(/=+$/, '');
      const bytes = [];
      let bits = 0, buffer = 0;
      for (const character of source) {
        buffer = (buffer << 6) | alphabet.indexOf(character);
        bits += 6;
        if (bits >= 8) {
          bits -= 8;
          bytes.push((buffer >> bits) & 255);
        }
      }
      return Uint8Array.from(bytes);
    }
    if (Array.isArray(value)) return value.map(decode);
    for (const key of Object.keys(value)) value[key] = decode(value[key]);
    return value;
  };
  const parseEnvelope = encoded => {
    const envelope = JSON.parse(encoded);
    if (!envelope.ok) {
      const error = envelope.kind === 'type' ? new TypeError(envelope.error) : new Error(envelope.error);
      throw error;
    }
    return decode(envelope.value);
  };
  const syncCall = (id, method, args) =>
    parseEnvelope(__steadwright_sync_call(id, method, encode(args)));
  const asyncCall = async (id, method, args) =>
    parseEnvelope(await __steadwright_call(id, method, encode(args)));
  function makeProxy(id, type) {
    const key = `${id}`;
    if (proxies.has(key)) return proxies.get(key);
    const target = {};
    Object.defineProperties(target, {
      __steadwrightRef: {value: id},
      __steadwrightType: {value: type},
    });
    const proxy = new Proxy(target, {
      get(_target, property) {
        if (property === '__steadwrightRef') return id;
        if (property === '__steadwrightType') return type;
        if (property === 'then') return undefined;
        if (property === 'toJSON') return () => ({__sw_ref: id, __sw_type: type});
        if (property === Symbol.toStringTag) return type;
        if (type === 'Page' && (property === 'keyboard' || property === 'mouse')) {
          return syncCall(id, String(property), []);
        }
        if (syncMethods.has(property))
          return (...args) => syncCall(id, String(property), args);
        return (...args) => asyncCall(id, String(property), args);
      }
    });
    proxies.set(key, proxy);
    return proxy;
  }
  const printable = value => {
    if (typeof value === 'string') return value;
    try { return JSON.stringify(value); } catch (_) { return String(value); }
  };
  globalThis.__steadwrightInstall = roots => {
    globalThis.browser = makeProxy(roots.browser, 'Browser');
    globalThis.context = makeProxy(roots.context, 'BrowserContext');
    globalThis.page = makeProxy(roots.page, 'Page');
  };
  globalThis.__steadwrightDecode = decode;
  globalThis.state = globalThis.state || {};
  globalThis.console = {
    log: (...args) => __steadwright_log('log', args.map(printable).join(' ')),
    warn: (...args) => __steadwright_log('warn', args.map(printable).join(' ')),
    error: (...args) => __steadwright_log('error', args.map(printable).join(' ')),
  };
  globalThis.stead = {
    credentials: {
      list: () => asyncCall(0, 'credentials.list', []),
      fill: (credential, usernameLocator, passwordLocator) =>
        asyncCall(0, 'credentials.fill', [credential, usernameLocator, passwordLocator]),
      fillTotp: (credential, fieldLocator) =>
        asyncCall(0, 'credentials.fillTotp', [credential, fieldLocator]),
    }
  };
  globalThis.__steadwrightSerializable = value => {
    if (value === undefined) return null;
    if (value instanceof Uint8Array) return Array.from(value);
    if (value && value.__steadwrightRef !== undefined)
      return {__sw_ref: value.__steadwrightRef, __sw_type: value.__steadwrightType};
    return value;
  };
})();
"#;

#[async_trait]
trait BrowserApi: Send + Sync {
    fn contexts(&self) -> Vec<BrowserContext>;
    fn default_context(&self) -> BrowserContext;
    async fn new_page(&self) -> steadwright::Result<Page>;
    async fn close(&self) -> steadwright::Result<()>;
}

#[async_trait]
impl BrowserApi for Browser {
    fn contexts(&self) -> Vec<BrowserContext> {
        self.contexts()
    }

    fn default_context(&self) -> BrowserContext {
        self.default_context()
    }

    async fn new_page(&self) -> steadwright::Result<Page> {
        self.new_page().await
    }

    async fn close(&self) -> steadwright::Result<()> {
        self.close().await
    }
}

struct BrowserConnection {
    browser: Arc<dyn BrowserApi>,
    raw: RawCdp,
    cdp: Connection,
}

enum BrowserConnectionState {
    Uninitialized,
    BootstrapFailed {
        cdp: Connection,
        raw: RawCdp,
        error: String,
    },
    Ready(Arc<BrowserConnection>),
    Failed(String),
}

/// Process-wide connection to the browser broker. The inherited fd pair is
/// preferred; a WebSocket endpoint is only a development/test fallback.
struct BrowserHandle;

impl BrowserHandle {
    async fn get() -> Result<Arc<BrowserConnection>, String> {
        static CONNECTION: OnceLock<Mutex<BrowserConnectionState>> = OnceLock::new();
        let slot = CONNECTION.get_or_init(|| Mutex::new(BrowserConnectionState::Uninitialized));
        let mut guard = slot.lock().await;
        match &*guard {
            BrowserConnectionState::Ready(connection) => {
                if let Some(reason) = connection.cdp.closed_reason() {
                    let error = closed_connection_error(&reason);
                    *guard = BrowserConnectionState::Failed(error.clone());
                    return Err(error);
                }
                return Ok(connection.clone());
            }
            BrowserConnectionState::Failed(error) => return Err(error.clone()),
            BrowserConnectionState::BootstrapFailed { .. }
            | BrowserConnectionState::Uninitialized => {}
        }

        let (cdp, raw) = match &*guard {
            BrowserConnectionState::BootstrapFailed {
                cdp, raw, error, ..
            } => {
                if let Some(reason) = cdp.closed_reason() {
                    let error = closed_connection_error(&reason);
                    *guard = BrowserConnectionState::Failed(error.clone());
                    return Err(error);
                }
                tracing::info!(
                    previous_error = %error,
                    "retrying steadwright bootstrap on the existing browser connection"
                );
                (cdp.clone(), raw.clone())
            }
            BrowserConnectionState::Uninitialized => {
                let routed = if inherited_stead_pipe_pair() {
                    FdPairTransport::from_raw_fds(3, 4)
                        .map(route_transport)
                        .map_err(|_| UNAVAILABLE.to_string())
                } else if let Ok(url) = std::env::var("STEADWRIGHT_WS_URL") {
                    if url.trim().is_empty() {
                        Err(UNAVAILABLE.to_string())
                    } else {
                        WebSocketTransport::connect(&url)
                            .await
                            .map(route_transport)
                            .map_err(unavailable_error)
                    }
                } else {
                    Err(UNAVAILABLE.to_string())
                };
                match routed {
                    Ok(routed) => routed,
                    Err(error) => {
                        *guard = BrowserConnectionState::Failed(error.clone());
                        return Err(error);
                    }
                }
            }
            BrowserConnectionState::Ready(_) | BrowserConnectionState::Failed(_) => unreachable!(),
        };

        match Browser::connect_connection(cdp.clone()).await {
            Ok(browser) => {
                let connection = Arc::new(BrowserConnection {
                    browser: Arc::new(browser),
                    raw,
                    cdp,
                });
                *guard = BrowserConnectionState::Ready(connection.clone());
                Ok(connection)
            }
            Err(error) => {
                let message = unavailable_error(&error);
                if let Some(reason) = cdp.closed_reason() {
                    let message = closed_connection_error(&reason);
                    *guard = BrowserConnectionState::Failed(message.clone());
                    return Err(message);
                }
                if matches!(
                    error,
                    steadwright::Error::Protocol(CdpError::Protocol { .. })
                ) {
                    *guard = BrowserConnectionState::BootstrapFailed {
                        cdp,
                        raw,
                        error: message.clone(),
                    };
                } else {
                    *guard = BrowserConnectionState::Failed(message.clone());
                }
                Err(message)
            }
        }
    }
}

fn unavailable_error(error: impl std::fmt::Display) -> String {
    format!("Browser control is unavailable: {error}")
}

fn closed_connection_error(reason: &str) -> String {
    format!("Browser control is unavailable: the browser connection was closed ({reason})")
}

fn pipe_stat(fd: libc::c_int) -> Option<libc::stat> {
    // SAFETY: fstat only writes to the provided initialized stat storage and
    // does not take ownership of the descriptor.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let result = unsafe { libc::fstat(fd, &mut stat) };
    (result == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFIFO).then_some(stat)
}

fn inherited_stead_pipe_pair() -> bool {
    let (Some(read), Some(write)) = (pipe_stat(3), pipe_stat(4)) else {
        return false;
    };
    // Cargo's jobserver commonly occupies fd 3 and 4 with the two ends of one
    // pipe. Stead supplies two distinct unidirectional pipes, so do not take
    // ownership unless the descriptors refer to different pipe objects.
    read.st_dev != write.st_dev || read.st_ino != write.st_ino
}

fn route_transport(transport: impl Transport) -> (Connection, RawCdp) {
    let (transport, raw) = RoutedTransport::new(transport);
    (Connection::new(transport), raw)
}

#[derive(Clone)]
struct RawCdp {
    transport: Arc<Mutex<Box<dyn Transport>>>,
    pending: RawPending,
    next_id: Arc<AtomicU64>,
}

impl RawCdp {
    async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("raw CDP mutex poisoned")
            .insert(id, tx);
        let encoded = json!({"id": id, "method": method, "params": params}).to_string();
        if let Err(error) = self.transport.lock().await.send(encoded).await {
            self.pending
                .lock()
                .expect("raw CDP mutex poisoned")
                .remove(&id);
            return Err(error.to_string());
        }
        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("CDP connection disconnected".into()),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("raw CDP mutex poisoned")
                    .remove(&id);
                Err("CDP request timed out".into())
            }
        }
    }
}

struct RoutedTransport {
    transport: Arc<Mutex<Box<dyn Transport>>>,
    incoming: Option<Incoming>,
}

impl RoutedTransport {
    fn new(transport: impl Transport) -> (Self, RawCdp) {
        let mut transport: Box<dyn Transport> = Box::new(transport);
        let mut source = transport.incoming();
        let transport = Arc::new(Mutex::new(transport));
        let pending = Arc::new(StdMutex::new(HashMap::<
            u64,
            oneshot::Sender<Result<Value, String>>,
        >::new()));
        let (forward_tx, forward_rx) = mpsc::channel(128);
        let router_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(message) = source.recv().await {
                match message {
                    Ok(encoded) => {
                        let parsed = serde_json::from_str::<Value>(&encoded).ok();
                        let raw_id = parsed
                            .as_ref()
                            .and_then(|value| value.get("id"))
                            .and_then(Value::as_u64)
                            .filter(|id| *id >= RAW_CDP_ID_BASE);
                        let routed = raw_id.and_then(|id| {
                            router_pending
                                .lock()
                                .expect("raw CDP mutex poisoned")
                                .remove(&id)
                        });
                        if let (Some(tx), Some(value)) = (routed, parsed) {
                            let result = if let Some(error) = value.get("error") {
                                Err(error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("CDP protocol error")
                                    .to_string())
                            } else {
                                Ok(value.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = tx.send(result);
                        } else if forward_tx.send(Ok(encoded)).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        let waiting = std::mem::take(
                            &mut *router_pending.lock().expect("raw CDP mutex poisoned"),
                        );
                        for (_, tx) in waiting {
                            let _ = tx.send(Err(message.clone()));
                        }
                        let _ = forward_tx.send(Err(error)).await;
                        break;
                    }
                }
            }
        });
        (
            Self {
                transport: transport.clone(),
                incoming: Some(forward_rx),
            },
            RawCdp {
                transport,
                pending,
                next_id: Arc::new(AtomicU64::new(RAW_CDP_ID_BASE)),
            },
        )
    }
}

#[async_trait]
impl Transport for RoutedTransport {
    async fn send(&self, message: String) -> Result<(), TransportError> {
        self.transport.lock().await.send(message).await
    }

    fn incoming(&mut self) -> Incoming {
        self.incoming
            .take()
            .expect("Transport::incoming may only be called once")
    }

    async fn close(&self) {
        self.transport.lock().await.close().await;
    }
}

#[derive(Clone)]
enum SteadwrightObject {
    Browser(Arc<dyn BrowserApi>),
    Context(BrowserContext),
    Page(Option<Page>),
    Frame(Frame),
    Locator(DescribedLocator),
    FrameLocator(DescribedFrameLocator),
    JsHandle(JsHandle),
    ElementHandle(ElementHandle),
    Response(Response),
    Dialog(Dialog),
    FileChooser(FileChooser),
    Download(Download),
    Keyboard(Keyboard),
    Mouse(Mouse),
}

#[derive(Clone)]
struct DescribedLocator {
    value: Locator,
    description: String,
}

impl std::ops::Deref for DescribedLocator {
    type Target = Locator;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

#[derive(Clone)]
struct DescribedFrameLocator {
    value: FrameLocator,
    description: String,
}

impl std::ops::Deref for DescribedFrameLocator {
    type Target = FrameLocator;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl SteadwrightObject {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Browser(_) => "Browser",
            Self::Context(_) => "BrowserContext",
            Self::Page(_) => "Page",
            Self::Frame(_) => "Frame",
            Self::Locator(_) => "Locator",
            Self::FrameLocator(_) => "FrameLocator",
            Self::JsHandle(_) => "JSHandle",
            Self::ElementHandle(_) => "ElementHandle",
            Self::Response(_) => "Response",
            Self::Dialog(_) => "Dialog",
            Self::FileChooser(_) => "FileChooser",
            Self::Download(_) => "Download",
            Self::Keyboard(_) => "Keyboard",
            Self::Mouse(_) => "Mouse",
        }
    }

    fn js_name(&self) -> &'static str {
        match self {
            Self::Browser(_) => "browser",
            Self::Context(_) => "context",
            Self::Page(_) => "page",
            Self::Frame(_) => "frame",
            Self::Locator(_) => "locator",
            Self::FrameLocator(_) => "frameLocator",
            Self::JsHandle(_) => "jsHandle",
            Self::ElementHandle(_) => "elementHandle",
            Self::Response(_) => "response",
            Self::Dialog(_) => "dialog",
            Self::FileChooser(_) => "fileChooser",
            Self::Download(_) => "download",
            Self::Keyboard(_) => "keyboard",
            Self::Mouse(_) => "mouse",
        }
    }
}

struct Registry {
    objects: HashMap<u64, SteadwrightObject>,
    next_id: u64,
    current_page: Option<Page>,
    current_context: Option<BrowserContext>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            objects: HashMap::new(),
            next_id: 1,
            current_page: None,
            current_context: None,
        }
    }
}

impl Registry {
    fn roots_for_execution(
        &mut self,
        browser: Arc<dyn BrowserApi>,
        context: BrowserContext,
        page: Option<Page>,
    ) -> Roots {
        if page.is_some() {
            self.current_page = page.clone();
        }
        self.current_context = Some(context.clone());
        let browser = self.insert(SteadwrightObject::Browser(browser));
        let context = self.insert(SteadwrightObject::Context(context));
        let page = self.insert(SteadwrightObject::Page(page));
        Roots {
            browser,
            context,
            page,
        }
    }

    fn insert(&mut self, object: SteadwrightObject) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(id, object);
        id
    }

    fn reference(&mut self, object: SteadwrightObject) -> Value {
        let type_name = object.type_name();
        let id = self.insert(object);
        json!({"__sw_ref": id, "__sw_type": type_name})
    }

    fn get(&self, id: u64) -> Result<SteadwrightObject, DispatchError> {
        self.objects
            .get(&id)
            .cloned()
            .ok_or_else(|| DispatchError::ordinary("Browser object is no longer available"))
    }

    fn locator_arg(&self, value: &Value) -> Result<Locator, DispatchError> {
        Ok(self.described_locator_arg(value)?.value)
    }

    fn described_locator_arg(&self, value: &Value) -> Result<DescribedLocator, DispatchError> {
        let id = value
            .get("__sw_ref_arg")
            .and_then(Value::as_u64)
            .ok_or_else(|| DispatchError::ordinary("Expected a Locator"))?;
        match self.get(id)? {
            SteadwrightObject::Locator(locator) => Ok(locator),
            _ => Err(DispatchError::ordinary("Expected a Locator")),
        }
    }
}

struct Roots {
    browser: u64,
    context: u64,
    page: u64,
}

#[derive(Debug)]
struct DispatchError {
    message: String,
    type_error: bool,
}

impl DispatchError {
    fn ordinary(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            type_error: false,
        }
    }

    fn unknown(receiver: &SteadwrightObject, method: &str) -> Self {
        Self {
            message: format!("{}.{} is not a function", receiver.js_name(), method),
            type_error: true,
        }
    }
}

impl From<steadwright::Error> for DispatchError {
    fn from(error: steadwright::Error) -> Self {
        Self::ordinary(error.to_string())
    }
}

#[derive(Clone)]
struct CapturedImage {
    data: String,
    hash: [u8; 32],
    mime_type: String,
}

#[derive(Clone)]
struct BrowserEventSink {
    tx: mpsc::UnboundedSender<ResponseEnvelope>,
    session_id: String,
    request_id: String,
}

impl BrowserEventSink {
    fn emit(&self, tool_call_id: impl Into<String>, status: &str, message: &str) {
        let _ = self.tx.send(ResponseEnvelope::session_event(
            Some(self.request_id.clone()),
            self.session_id.clone(),
            BrainEvent::ToolStatus(ToolStatus {
                tool_call_id: tool_call_id.into(),
                status: status.to_string(),
                message: Some(message.to_string()),
            }),
        ));
    }
}

struct OperationProgress<'a> {
    sink: &'a BrowserEventSink,
    tool_call_id: String,
    message: String,
    finished: bool,
}

impl<'a> OperationProgress<'a> {
    fn start(
        sink: &'a BrowserEventSink,
        parent_tool_call_id: &str,
        sequence: u64,
        message: String,
    ) -> Self {
        let tool_call_id = format!("{parent_tool_call_id}:op:{sequence}");
        sink.emit(&tool_call_id, "running", &message);
        Self {
            sink,
            tool_call_id,
            message,
            finished: false,
        }
    }

    fn finish(mut self, succeeded: bool) {
        self.sink.emit(
            &self.tool_call_id,
            if succeeded { "completed" } else { "failed" },
            &self.message,
        );
        self.finished = true;
    }
}

impl Drop for OperationProgress<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.sink.emit(&self.tool_call_id, "failed", &self.message);
        }
    }
}

struct ExecutionHost {
    registry: Arc<StdMutex<Registry>>,
    bridge: Arc<dyn BrowserToolBridge>,
    raw: RawCdp,
    tool_call_id: String,
    event_sink: Option<BrowserEventSink>,
    operation_sequence: AtomicU64,
    credential_sequence: AtomicU64,
    cancel: CancellationToken,
    images: Arc<StdMutex<Vec<CapturedImage>>>,
    omitted_images: Arc<StdMutex<usize>>,
    marked_locators: StdMutex<Vec<(String, Locator)>>,
}

impl ExecutionHost {
    async fn dispatch(
        &self,
        receiver_id: u64,
        method: &str,
        args: Value,
    ) -> Result<Value, DispatchError> {
        let receiver = if receiver_id == 0 {
            None
        } else {
            Some(
                self.registry
                    .lock()
                    .expect("registry mutex poisoned")
                    .get(receiver_id)?,
            )
        };
        let progress = self
            .event_sink
            .as_ref()
            .zip(operation_message(
                operation_target(receiver.as_ref()),
                method,
                &args,
            ))
            .map(|(sink, message)| {
                OperationProgress::start(
                    sink,
                    &self.tool_call_id,
                    self.operation_sequence.fetch_add(1, Ordering::Relaxed),
                    message,
                )
            });
        let result = match receiver {
            Some(receiver) => self.dispatch_object(receiver, method, args).await,
            None => self.dispatch_credentials(method, args).await,
        };
        if let Some(progress) = progress {
            progress.finish(result.is_ok());
        }
        result
    }

    fn dispatch_sync(
        &self,
        receiver_id: u64,
        method: &str,
        args: Value,
    ) -> Result<Value, DispatchError> {
        let mut registry = self.registry.lock().expect("registry mutex poisoned");
        let receiver = registry.get(receiver_id)?;
        if let SteadwrightObject::Page(Some(page)) = &receiver {
            registry.current_page = Some(page.clone());
        }
        match (&receiver, method) {
            (SteadwrightObject::Browser(browser), "contexts") => {
                return Ok(Value::Array(
                    browser
                        .contexts()
                        .into_iter()
                        .map(|value| registry.reference(SteadwrightObject::Context(value)))
                        .collect(),
                ));
            }
            (SteadwrightObject::Context(context), "pages") => {
                return Ok(Value::Array(
                    context
                        .pages()
                        .into_iter()
                        .map(|value| registry.reference(SteadwrightObject::Page(Some(value))))
                        .collect(),
                ));
            }
            (SteadwrightObject::Page(Some(page)), "url") => return Ok(json!(page.url())),
            (SteadwrightObject::Page(Some(page)), "isClosed") => {
                return Ok(json!(page.is_closed()));
            }
            (SteadwrightObject::Page(Some(page)), "viewportSize") => {
                return Ok(match page.viewport_size() {
                    Some(v) => json!({"width":v.width,"height":v.height}),
                    None => Value::Null,
                });
            }
            (SteadwrightObject::Page(Some(page)), "frames") => {
                return Ok(Value::Array(
                    page.frames()
                        .into_iter()
                        .map(|value| registry.reference(SteadwrightObject::Frame(value)))
                        .collect(),
                ));
            }
            (SteadwrightObject::Page(Some(page)), "setDefaultTimeout") => {
                page.set_default_timeout(u64_arg(&args, 0)?);
                return Ok(Value::Null);
            }
            (SteadwrightObject::Page(Some(page)), "setDefaultNavigationTimeout") => {
                page.set_default_navigation_timeout(u64_arg(&args, 0)?);
                return Ok(Value::Null);
            }
            (SteadwrightObject::Frame(frame), "url") => return Ok(json!(frame.url())),
            (SteadwrightObject::Frame(frame), "name") => return Ok(json!(frame.name())),
            (SteadwrightObject::Frame(frame), "isDetached") => {
                return Ok(json!(frame.is_detached()));
            }
            (SteadwrightObject::Frame(frame), "parentFrame") => {
                return Ok(match frame.parent_frame() {
                    Some(v) => registry.reference(SteadwrightObject::Frame(v)),
                    None => Value::Null,
                });
            }
            (SteadwrightObject::Frame(frame), "childFrames") => {
                return Ok(Value::Array(
                    frame
                        .child_frames()
                        .into_iter()
                        .map(|value| registry.reference(SteadwrightObject::Frame(value)))
                        .collect(),
                ));
            }
            (SteadwrightObject::Response(response), "url") => return Ok(json!(response.url)),
            (SteadwrightObject::Response(response), "status") => return Ok(json!(response.status)),
            (SteadwrightObject::Response(response), "ok") => return Ok(json!(response.ok)),
            (SteadwrightObject::Response(response), "headers") => {
                return Ok(json!(response.headers));
            }
            (SteadwrightObject::JsHandle(handle), "asElement") => {
                return Ok(match handle.as_element() {
                    Some(v) => registry.reference(SteadwrightObject::ElementHandle(v)),
                    None => Value::Null,
                });
            }
            (SteadwrightObject::FileChooser(chooser), "isMultiple") => {
                return Ok(json!(chooser.is_multiple));
            }
            (SteadwrightObject::FileChooser(chooser), "element") => {
                return Ok(
                    registry.reference(SteadwrightObject::ElementHandle(chooser.element.clone()))
                );
            }
            (SteadwrightObject::Download(download), "url") => return Ok(json!(download.url)),
            (SteadwrightObject::Download(download), "suggestedFilename") => {
                return Ok(json!(download.suggested_filename));
            }
            (SteadwrightObject::Dialog(dialog), "type") => {
                return Ok(json!(dialog_type_name(dialog.type_)));
            }
            (SteadwrightObject::Dialog(dialog), "message") => return Ok(json!(dialog.message)),
            (SteadwrightObject::Dialog(dialog), "defaultValue") => {
                return Ok(json!(dialog.default_value));
            }
            _ => {}
        }
        let object = match (&receiver, method) {
            (SteadwrightObject::Page(Some(page)), "keyboard") => {
                SteadwrightObject::Keyboard(page.keyboard())
            }
            (SteadwrightObject::Page(Some(page)), "mouse") => {
                SteadwrightObject::Mouse(page.mouse())
            }
            (SteadwrightObject::Page(Some(page)), "mainFrame") => {
                SteadwrightObject::Frame(page.main_frame())
            }
            (SteadwrightObject::Page(Some(page)), "locator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::Locator(DescribedLocator {
                    value: page.locator(selector),
                    description: format!("locator({})", js_string(selector)),
                })
            }
            (SteadwrightObject::Page(Some(page)), "frameLocator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::FrameLocator(DescribedFrameLocator {
                    value: page.frame_locator(selector),
                    description: format!("frameLocator({})", js_string(selector)),
                })
            }
            (SteadwrightObject::Frame(frame), "locator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::Locator(DescribedLocator {
                    value: frame.locator(selector),
                    description: format!("locator({})", js_string(selector)),
                })
            }
            (SteadwrightObject::Frame(frame), "frameLocator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::FrameLocator(DescribedFrameLocator {
                    value: frame.frame_locator(selector),
                    description: format!("frameLocator({})", js_string(selector)),
                })
            }
            (SteadwrightObject::Locator(locator), "locator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.locator(selector),
                    description: format!(
                        "{}.locator({})",
                        locator.description,
                        js_string(selector)
                    ),
                })
            }
            (SteadwrightObject::Locator(locator), "frameLocator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::FrameLocator(DescribedFrameLocator {
                    value: locator.frame_locator(selector),
                    description: format!(
                        "{}.frameLocator({})",
                        locator.description,
                        js_string(selector)
                    ),
                })
            }
            (SteadwrightObject::Locator(locator), "contentFrame") => {
                SteadwrightObject::FrameLocator(DescribedFrameLocator {
                    value: locator.content_frame(),
                    description: format!("{}.contentFrame()", locator.description),
                })
            }
            (SteadwrightObject::Locator(locator), "first") => {
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.first(),
                    description: format!("{}.first()", locator.description),
                })
            }
            (SteadwrightObject::Locator(locator), "last") => {
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.last(),
                    description: format!("{}.last()", locator.description),
                })
            }
            (SteadwrightObject::Locator(locator), "nth") => {
                let index = i32_arg(&args, 0)?;
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.nth(index),
                    description: format!("{}.nth({index})", locator.description),
                })
            }
            (SteadwrightObject::Locator(locator), "filter") => {
                let options = arg(&args, 0).unwrap_or(&Value::Null);
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.filter(locator_filter(&registry, options)?)?,
                    description: format!("{}.filter({})", locator.description, js_value(options)),
                })
            }
            (SteadwrightObject::Locator(locator), "and") => {
                let other = registry.described_locator_arg(arg_required(&args, 0)?)?;
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.and_(&other.value)?,
                    description: format!("{}.and({})", locator.description, other.description),
                })
            }
            (SteadwrightObject::Locator(locator), "or") => {
                let other = registry.described_locator_arg(arg_required(&args, 0)?)?;
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.or_(&other.value)?,
                    description: format!("{}.or({})", locator.description, other.description),
                })
            }
            (SteadwrightObject::FrameLocator(locator), "locator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator.locator(selector),
                    description: format!(
                        "{}.locator({})",
                        locator.description,
                        js_string(selector)
                    ),
                })
            }
            (SteadwrightObject::FrameLocator(locator), "frameLocator") => {
                let selector = string_arg(&args, 0)?;
                SteadwrightObject::FrameLocator(DescribedFrameLocator {
                    value: locator.frame_locator(selector),
                    description: format!(
                        "{}.frameLocator({})",
                        locator.description,
                        js_string(selector)
                    ),
                })
            }
            (_, "getByRole") => {
                let role = string_arg(&args, 0)?;
                let options = by_role_options(arg(&args, 1).unwrap_or(&Value::Null))?;
                let value = match &receiver {
                    SteadwrightObject::Page(Some(v)) => v.get_by_role(role, options),
                    SteadwrightObject::Frame(v) => v.get_by_role(role, options),
                    SteadwrightObject::Locator(v) => v.get_by_role(role, options),
                    SteadwrightObject::FrameLocator(v) => v.get_by_role(role, options),
                    _ => return Err(DispatchError::unknown(&receiver, method)),
                };
                SteadwrightObject::Locator(DescribedLocator {
                    value,
                    description: locator_method_description(&receiver, method, &args)?,
                })
            }
            (
                _,
                "getByText" | "getByLabel" | "getByPlaceholder" | "getByAltText" | "getByTitle"
                | "getByTestId",
            ) => {
                let text = text_match(arg_required(&args, 0)?)?;
                let exact = arg(&args, 1)
                    .and_then(|v| v.get("exact").or(Some(v)))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                SteadwrightObject::Locator(DescribedLocator {
                    value: locator_text_method(&receiver, method, text, exact)?,
                    description: locator_method_description(&receiver, method, &args)?,
                })
            }
            _ => return Err(DispatchError::unknown(&receiver, method)),
        };
        Ok(registry.reference(object))
    }

    async fn dispatch_object(
        &self,
        receiver: SteadwrightObject,
        method: &str,
        args: Value,
    ) -> Result<Value, DispatchError> {
        match receiver.clone() {
            SteadwrightObject::Browser(browser) => match method {
                "newPage" => {
                    let page = browser.new_page().await?;
                    let context = browser.default_context();
                    self.ref_page(page, Some(context))
                }
                "close" => {
                    browser.close().await?;
                    Ok(Value::Null)
                }
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::Context(context) => match method {
                "newPage" => {
                    let page = context.new_page().await?;
                    self.ref_page(page, Some(context))
                }
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::Page(page) => {
                self.dispatch_page(page, method, args, &receiver).await
            }
            SteadwrightObject::Frame(frame) => {
                self.dispatch_frame(frame, method, args, &receiver).await
            }
            SteadwrightObject::Locator(locator) => {
                self.dispatch_locator(locator, method, args, &receiver)
                    .await
            }
            SteadwrightObject::JsHandle(handle) => match method {
                "jsonValue" => Ok(handle.json_value().await?.to_json()),
                "dispose" => {
                    handle.dispose().await?;
                    Ok(Value::Null)
                }
                "evaluate" => Ok(handle
                    .evaluate_json(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                    .await?),
                "evaluateHandle" => self.ref_value(SteadwrightObject::JsHandle(
                    handle
                        .evaluate_handle(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                        .await?,
                )),
                "asElement" => match handle.as_element() {
                    Some(element) => self.ref_value(SteadwrightObject::ElementHandle(element)),
                    None => Ok(Value::Null),
                },
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::ElementHandle(handle) => match method {
                "screenshot" => self.image_result(handle.screenshot().await?, "image/png"),
                "jsonValue" => Ok(handle.0.json_value().await?.to_json()),
                "dispose" => {
                    handle.0.dispose().await?;
                    Ok(Value::Null)
                }
                "evaluate" => Ok(handle
                    .0
                    .evaluate_json(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                    .await?),
                "evaluateHandle" => self.ref_value(SteadwrightObject::JsHandle(
                    handle
                        .0
                        .evaluate_handle(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                        .await?,
                )),
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::Response(response) => match method {
                "url" => Ok(json!(response.url)),
                "status" => Ok(json!(response.status)),
                "ok" => Ok(json!(response.ok)),
                "headers" => Ok(json!(response.headers)),
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::Dialog(dialog) => match method {
                "accept" => {
                    dialog.accept(optional_string_arg(&args, 0)).await?;
                    Ok(Value::Null)
                }
                "dismiss" => {
                    dialog.dismiss().await?;
                    Ok(Value::Null)
                }
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::FileChooser(chooser) => match method {
                "setFiles" => {
                    chooser.set_files(path_args(&args, 0)?).await?;
                    Ok(Value::Null)
                }
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::Download(download) => match method {
                "path" => Ok(json!(download.path().await?.to_string_lossy())),
                _ => Err(DispatchError::unknown(&receiver, method)),
            },
            SteadwrightObject::Keyboard(keyboard) => {
                self.dispatch_keyboard(keyboard, method, args, &receiver)
                    .await
            }
            SteadwrightObject::Mouse(mouse) => {
                self.dispatch_mouse(mouse, method, args, &receiver).await
            }
            SteadwrightObject::FrameLocator(_) => Err(DispatchError::unknown(&receiver, method)),
        }
    }

    fn ref_value(&self, object: SteadwrightObject) -> Result<Value, DispatchError> {
        Ok(self
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .reference(object))
    }

    fn ref_page(
        &self,
        page: Page,
        context: Option<BrowserContext>,
    ) -> Result<Value, DispatchError> {
        let mut registry = self.registry.lock().expect("registry mutex poisoned");
        registry.current_page = Some(page.clone());
        if let Some(context) = context {
            registry.current_context = Some(context);
        }
        Ok(registry.reference(SteadwrightObject::Page(Some(page))))
    }

    async fn dispatch_page(
        &self,
        page: Option<Page>,
        method: &str,
        args: Value,
        receiver: &SteadwrightObject,
    ) -> Result<Value, DispatchError> {
        let page = match page {
            Some(page) => page,
            None if method == "goto" => {
                let context = self
                    .registry
                    .lock()
                    .expect("registry mutex poisoned")
                    .current_context
                    .clone()
                    .ok_or_else(|| DispatchError::ordinary(UNAVAILABLE))?;
                let page = context.new_page().await?;
                let mut registry = self.registry.lock().expect("registry mutex poisoned");
                registry.current_page = Some(page.clone());
                for object in registry.objects.values_mut() {
                    if matches!(object, SteadwrightObject::Page(None)) {
                        *object = SteadwrightObject::Page(Some(page.clone()));
                    }
                }
                page
            }
            None => {
                return Err(DispatchError::ordinary(
                    "No current browser page. Call page.goto(url) first.",
                ));
            }
        };
        self.registry
            .lock()
            .expect("registry mutex poisoned")
            .current_page = Some(page.clone());
        match method {
            "goto" => self.optional_response(
                page.goto(string_arg(&args, 0)?, goto_options(arg(&args, 1)))
                    .await?,
            ),
            "title" => Ok(json!(page.title().await?)),
            "content" => Ok(json!(page.content().await?)),
            "url" => Ok(json!(page.url())),
            "isClosed" => Ok(json!(page.is_closed())),
            "bringToFront" => {
                page.bring_to_front().await?;
                Ok(Value::Null)
            }
            "close" => {
                page.close().await?;
                Ok(Value::Null)
            }
            "reload" => self.optional_response(page.reload(goto_options(arg(&args, 0))).await?),
            "goBack" => self.optional_response(page.go_back(goto_options(arg(&args, 0))).await?),
            "goForward" => {
                self.optional_response(page.go_forward(goto_options(arg(&args, 0))).await?)
            }
            "evaluate" => Ok(page
                .evaluate_json(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                .await?),
            "evaluateHandle" => self.ref_value(SteadwrightObject::JsHandle(
                page.evaluate_handle(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                    .await?,
            )),
            "ariaSnapshot" => Ok(json!(
                page.aria_snapshot(aria_options(arg(&args, 0))).await?
            )),
            "screenshot" => {
                let options = screenshot_options(arg(&args, 0));
                let mime_type = screenshot_mime_type(options.format);
                self.image_result(page.screenshot(options).await?, mime_type)
            }
            "waitForTimeout" => {
                page.wait_for_timeout(u64_arg(&args, 0)?).await?;
                Ok(Value::Null)
            }
            "waitForFunction" => self.ref_value(SteadwrightObject::JsHandle(
                page.wait_for_function(
                    function_arg(&args, 0)?,
                    self.call_arg(&args, 1)?,
                    wait_function_options(arg(&args, 2)),
                )
                .await?,
            )),
            "waitForSelector" => match page
                .wait_for_selector(string_arg(&args, 0)?, wait_options(arg(&args, 1)))
                .await?
            {
                Some(v) => self.ref_value(SteadwrightObject::ElementHandle(v)),
                None => Ok(Value::Null),
            },
            "waitForLoadState" => {
                page.wait_for_load_state(load_state(arg(&args, 0)), timeout_from(arg(&args, 1)))
                    .await?;
                Ok(Value::Null)
            }
            "waitForURL" => {
                page.wait_for_url(
                    UrlMatcher::Glob(string_arg(&args, 0)?.to_string()),
                    wait_url_options(arg(&args, 1)),
                )
                .await?;
                Ok(Value::Null)
            }
            "waitForResponse" => {
                let response = page
                    .wait_for_response(
                        UrlMatcher::Glob(string_arg(&args, 0)?.to_string()),
                        timeout_from(arg(&args, 1)),
                    )
                    .await?;
                self.ref_value(SteadwrightObject::Response(response))
            }
            "waitForRequest" => {
                let request = page
                    .wait_for_request(
                        UrlMatcher::Glob(string_arg(&args, 0)?.to_string()),
                        timeout_from(arg(&args, 1)),
                    )
                    .await?;
                Ok(
                    json!({"url":request.url,"method":request.method,"headers":request.headers,"postData":request.post_data}),
                )
            }
            "waitForEvent" => {
                let event = page
                    .wait_for_event(string_arg(&args, 0)?, timeout_from(arg(&args, 1)))
                    .await?;
                match event {
                    PageEvent::Download(value) => {
                        self.ref_value(SteadwrightObject::Download(value))
                    }
                    PageEvent::Dialog(value) => self.ref_value(SteadwrightObject::Dialog(value)),
                    PageEvent::FileChooser(value) => {
                        self.ref_value(SteadwrightObject::FileChooser(value))
                    }
                    PageEvent::Popup(value) => self.ref_page(value, None),
                }
            }
            "setViewportSize" => {
                let size = arg_required(&args, 0)?;
                page.set_viewport_size(value_u32(size, "width")?, value_u32(size, "height")?)
                    .await?;
                Ok(Value::Null)
            }
            _ => Err(DispatchError::unknown(receiver, method)),
        }
    }

    async fn dispatch_frame(
        &self,
        frame: Frame,
        method: &str,
        args: Value,
        receiver: &SteadwrightObject,
    ) -> Result<Value, DispatchError> {
        match method {
            "url" => Ok(json!(frame.url())),
            "name" => Ok(json!(frame.name())),
            "isDetached" => Ok(json!(frame.is_detached())),
            "parentFrame" => match frame.parent_frame() {
                Some(v) => self.ref_value(SteadwrightObject::Frame(v)),
                None => Ok(Value::Null),
            },
            "childFrames" => Ok(Value::Array(
                frame
                    .child_frames()
                    .into_iter()
                    .map(|v| self.ref_value(SteadwrightObject::Frame(v)))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            "goto" => self.optional_response(
                frame
                    .goto(string_arg(&args, 0)?, goto_options(arg(&args, 1)))
                    .await?,
            ),
            "evaluate" => Ok(frame
                .evaluate_json(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                .await?),
            "evaluateHandle" => self.ref_value(SteadwrightObject::JsHandle(
                frame
                    .evaluate_handle(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                    .await?,
            )),
            "ariaSnapshot" => Ok(json!(
                frame.aria_snapshot(aria_options(arg(&args, 0))).await?
            )),
            "waitForFunction" => self.ref_value(SteadwrightObject::JsHandle(
                frame
                    .wait_for_function(
                        function_arg(&args, 0)?,
                        self.call_arg(&args, 1)?,
                        wait_function_options(arg(&args, 2)),
                    )
                    .await?,
            )),
            "waitForSelector" => match frame
                .wait_for_selector(string_arg(&args, 0)?, wait_options(arg(&args, 1)))
                .await?
            {
                Some(v) => self.ref_value(SteadwrightObject::ElementHandle(v)),
                None => Ok(Value::Null),
            },
            _ => Err(DispatchError::unknown(receiver, method)),
        }
    }

    async fn dispatch_locator(
        &self,
        locator: DescribedLocator,
        method: &str,
        args: Value,
        receiver: &SteadwrightObject,
    ) -> Result<Value, DispatchError> {
        let options = || action_options(arg(&args, 0));
        match method {
            "count" => Ok(json!(locator.count().await?)),
            "all" => Ok(Value::Array(
                locator
                    .all()
                    .await?
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| {
                        self.ref_value(SteadwrightObject::Locator(DescribedLocator {
                            value,
                            description: format!("{}.nth({index})", locator.description),
                        }))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            "elementHandle" => self.ref_value(SteadwrightObject::ElementHandle(
                locator.element_handle().await?,
            )),
            "elementHandles" => Ok(Value::Array(
                locator
                    .element_handles()
                    .await?
                    .into_iter()
                    .map(|v| self.ref_value(SteadwrightObject::ElementHandle(v)))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            "evaluate" => Ok(locator
                .evaluate(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                .await?
                .to_json()),
            "evaluateAll" => Ok(locator
                .evaluate_all(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                .await?
                .to_json()),
            "evaluateHandle" => self.ref_value(SteadwrightObject::JsHandle(
                locator
                    .evaluate_handle(function_arg(&args, 0)?, self.call_arg(&args, 1)?)
                    .await?,
            )),
            "allInnerTexts" => Ok(json!(locator.all_inner_texts().await?)),
            "allTextContents" => Ok(json!(locator.all_text_contents().await?)),
            "innerText" => Ok(json!(locator.inner_text().await?)),
            "textContent" => Ok(json!(locator.text_content().await?)),
            "innerHTML" => Ok(json!(locator.inner_html().await?)),
            "inputValue" => Ok(json!(locator.input_value().await?)),
            "getAttribute" => Ok(json!(locator.get_attribute(string_arg(&args, 0)?).await?)),
            "boundingBox" => match locator.bounding_box().await? {
                Some(value) => {
                    Ok(json!({"x":value.x,"y":value.y,"width":value.width,"height":value.height}))
                }
                None => Ok(Value::Null),
            },
            "isVisible" => Ok(json!(locator.is_visible().await?)),
            "isHidden" => Ok(json!(locator.is_hidden().await?)),
            "isEnabled" => Ok(json!(locator.is_enabled().await?)),
            "isDisabled" => Ok(json!(locator.is_disabled().await?)),
            "isChecked" => Ok(json!(locator.is_checked().await?)),
            "isEditable" => Ok(json!(locator.is_editable().await?)),
            "click" => {
                locator.click(options()).await?;
                Ok(Value::Null)
            }
            "dblclick" => {
                locator.dblclick(options()).await?;
                Ok(Value::Null)
            }
            "hover" => {
                locator.hover(options()).await?;
                Ok(Value::Null)
            }
            "tap" => {
                locator.tap(options()).await?;
                Ok(Value::Null)
            }
            "fill" => {
                locator
                    .fill(string_arg(&args, 0)?, action_options(arg(&args, 1)))
                    .await?;
                Ok(Value::Null)
            }
            "clear" => {
                locator.clear(options()).await?;
                Ok(Value::Null)
            }
            "focus" => {
                locator.focus(timeout_from(arg(&args, 0))).await?;
                Ok(Value::Null)
            }
            "blur" => {
                locator.blur(timeout_from(arg(&args, 0))).await?;
                Ok(Value::Null)
            }
            "selectText" => {
                locator.select_text(options()).await?;
                Ok(Value::Null)
            }
            "type" => {
                locator
                    .type_text(string_arg(&args, 0)?, delay_from(arg(&args, 1)))
                    .await?;
                Ok(Value::Null)
            }
            "press" => {
                locator
                    .press(string_arg(&args, 0)?, delay_from(arg(&args, 1)))
                    .await?;
                Ok(Value::Null)
            }
            "check" => {
                locator.check(options()).await?;
                Ok(Value::Null)
            }
            "uncheck" => {
                locator.uncheck(options()).await?;
                Ok(Value::Null)
            }
            "setChecked" => {
                locator
                    .set_checked(bool_arg(&args, 0)?, action_options(arg(&args, 1)))
                    .await?;
                Ok(Value::Null)
            }
            "selectOption" => Ok(json!(
                locator
                    .select_option(
                        select_values(arg_required(&args, 0)?)?,
                        action_options(arg(&args, 1))
                    )
                    .await?
            )),
            "setInputFiles" => {
                locator
                    .set_input_files(path_args(&args, 0)?, action_options(arg(&args, 1)))
                    .await?;
                Ok(Value::Null)
            }
            "dispatchEvent" => {
                locator
                    .dispatch_event(string_arg(&args, 0)?, json_arg(&args, 1))
                    .await?;
                Ok(Value::Null)
            }
            "scrollIntoViewIfNeeded" => {
                locator
                    .scroll_into_view_if_needed(timeout_from(arg(&args, 0)))
                    .await?;
                Ok(Value::Null)
            }
            "dragTo" => {
                let target = self
                    .registry
                    .lock()
                    .expect("registry mutex poisoned")
                    .locator_arg(arg_required(&args, 0)?)?;
                locator
                    .drag_to(&target, action_options(arg(&args, 1)))
                    .await?;
                Ok(Value::Null)
            }
            "waitFor" => {
                locator.wait_for(wait_options(arg(&args, 0))).await?;
                Ok(Value::Null)
            }
            "ariaSnapshot" => Ok(json!(
                locator.aria_snapshot(aria_options(arg(&args, 0))).await?
            )),
            "highlight" => {
                locator.highlight().await?;
                Ok(Value::Null)
            }
            "describe" => Ok(json!(locator.describe().await?)),
            "screenshot" => {
                let options = screenshot_options(arg(&args, 0));
                let mime_type = screenshot_mime_type(options.format);
                self.image_result(locator.screenshot(options).await?, mime_type)
            }
            _ => Err(DispatchError::unknown(receiver, method)),
        }
    }

    async fn dispatch_keyboard(
        &self,
        keyboard: Keyboard,
        method: &str,
        args: Value,
        receiver: &SteadwrightObject,
    ) -> Result<Value, DispatchError> {
        match method {
            "down" => keyboard.down(string_arg(&args, 0)?).await?,
            "up" => keyboard.up(string_arg(&args, 0)?).await?,
            "press" => keyboard.press(string_arg(&args, 0)?).await?,
            "type" => keyboard.type_text(string_arg(&args, 0)?).await?,
            "insertText" => keyboard.insert_text(string_arg(&args, 0)?).await?,
            _ => return Err(DispatchError::unknown(receiver, method)),
        }
        Ok(Value::Null)
    }

    async fn dispatch_mouse(
        &self,
        mouse: Mouse,
        method: &str,
        args: Value,
        receiver: &SteadwrightObject,
    ) -> Result<Value, DispatchError> {
        match method {
            "move" => {
                mouse
                    .move_to(
                        f64_arg(&args, 0)?,
                        f64_arg(&args, 1)?,
                        arg(&args, 2)
                            .and_then(|v| v.get("steps"))
                            .and_then(Value::as_u64)
                            .unwrap_or(1) as u32,
                    )
                    .await?
            }
            "click" => {
                mouse
                    .click(
                        f64_arg(&args, 0)?,
                        f64_arg(&args, 1)?,
                        click_options(arg(&args, 2)),
                    )
                    .await?
            }
            "dblclick" => {
                mouse
                    .dblclick(
                        f64_arg(&args, 0)?,
                        f64_arg(&args, 1)?,
                        click_options(arg(&args, 2)),
                    )
                    .await?
            }
            "down" => {
                mouse
                    .down(
                        mouse_button(arg(&args, 0).and_then(|v| v.get("button"))),
                        arg(&args, 0)
                            .and_then(|v| v.get("clickCount"))
                            .and_then(Value::as_u64)
                            .unwrap_or(1) as u8,
                    )
                    .await?
            }
            "up" => {
                mouse
                    .up(
                        mouse_button(arg(&args, 0).and_then(|v| v.get("button"))),
                        arg(&args, 0)
                            .and_then(|v| v.get("clickCount"))
                            .and_then(Value::as_u64)
                            .unwrap_or(1) as u8,
                    )
                    .await?
            }
            "wheel" => mouse.wheel(f64_arg(&args, 0)?, f64_arg(&args, 1)?).await?,
            _ => return Err(DispatchError::unknown(receiver, method)),
        }
        Ok(Value::Null)
    }

    fn image_result(&self, bytes: Vec<u8>, mime_type: &str) -> Result<Value, DispatchError> {
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        let mut images = self.images.lock().expect("image mutex poisoned");
        if !images.iter().any(|image| image.hash == hash) {
            if images.len() < MAX_IMAGES {
                images.push(CapturedImage {
                    data: data.clone(),
                    hash,
                    mime_type: mime_type.to_string(),
                });
            } else {
                *self.omitted_images.lock().expect("image mutex poisoned") += 1;
            }
        }
        Ok(json!({"__sw_bytes": data}))
    }

    fn optional_response(&self, response: Option<Response>) -> Result<Value, DispatchError> {
        match response {
            Some(response) => self.ref_value(SteadwrightObject::Response(response)),
            None => Ok(Value::Null),
        }
    }

    async fn dispatch_credentials(
        &self,
        method: &str,
        args: Value,
    ) -> Result<Value, DispatchError> {
        match method {
            "credentials.list" => {
                let page = self.current_page()?;
                let target = self.describe_target(&page).await?;
                let origin = page.evaluate_json("location.origin", Value::Null).await?;
                let result = self
                    .call_credential_bridge(
                        "list",
                        "browser.list_credentials",
                        json!({"tab_id": target.tab_id, "origin": origin}),
                    )
                    .await?;
                bridge_content(result)
            }
            "credentials.fill" => {
                let credential = arg_required(&args, 0)?.clone();
                let username = self.locator_from_arg(arg_required(&args, 1)?)?;
                let password = self.locator_from_arg(arg_required(&args, 2)?)?;
                let page = self.current_page()?;
                let target = self.describe_target(&page).await?;
                // The browser fills both fields from one marked element (it
                // finds the sibling username field itself), so only the
                // password field is marked; the username locator is resolved
                // to validate it exists and to keep Playwright's signature.
                username.count().await.map_err(DispatchError::from)?;
                let password_marker = self.mark_locator(&password).await?;
                let result = self
                    .call_credential_bridge(
                        "fill",
                        "browser.fill_credential",
                        json!({
                            "tab_id": target.tab_id,
                            "frame_token": target.frame_token,
                            "credential": credential_handle(&credential)?,
                            "marker": password_marker,
                        }),
                    )
                    .await;
                self.unmark_locator(&password_marker).await;
                bridge_content(result?)
            }
            "credentials.fillTotp" => {
                let credential = arg_required(&args, 0)?.clone();
                let locator = self.locator_from_arg(arg_required(&args, 1)?)?;
                let page = self.current_page()?;
                let target = self.describe_target(&page).await?;
                let marker = self.mark_locator(&locator).await?;
                let result = self
                    .call_credential_bridge(
                        "totp",
                        "browser.fill_totp",
                        json!({
                            "tab_id": target.tab_id,
                            "frame_token": target.frame_token,
                            "credential": credential_handle(&credential)?,
                            "marker": marker,
                        }),
                    )
                    .await;
                self.unmark_locator(&marker).await;
                bridge_content(result?)
            }
            _ => Err(DispatchError::ordinary(format!(
                "stead.{method} is not a function"
            ))),
        }
    }

    fn current_page(&self) -> Result<Page, DispatchError> {
        self.registry
            .lock()
            .expect("registry mutex poisoned")
            .current_page
            .clone()
            .ok_or_else(|| {
                DispatchError::ordinary("No current browser page. Call page.goto(url) first.")
            })
    }

    fn credential_call_id(&self, operation: &str) -> String {
        let sequence = self.credential_sequence.fetch_add(1, Ordering::Relaxed);
        format!("{}-credentials-{operation}-{sequence}", self.tool_call_id)
    }

    async fn call_credential_bridge(
        &self,
        operation: &str,
        name: &str,
        arguments: Value,
    ) -> Result<stead_brain_protocol::ToolResultPayload, DispatchError> {
        self.bridge
            .call_browser_tool(
                &self.credential_call_id(operation),
                name,
                arguments,
                self.cancel.clone(),
            )
            .await
            .map_err(|error| DispatchError::ordinary(error.to_string()))
    }

    async fn mark_locator(&self, locator: &Locator) -> Result<String, DispatchError> {
        let marker = mark_locator(locator).await?;
        self.marked_locators
            .lock()
            .expect("credential marker mutex poisoned")
            .push((marker.clone(), locator.clone()));
        Ok(marker)
    }

    async fn unmark_locator(&self, marker: &str) {
        let locator = {
            let mut marked = self
                .marked_locators
                .lock()
                .expect("credential marker mutex poisoned");
            marked
                .iter()
                .position(|(candidate, _)| candidate == marker)
                .map(|index| marked.swap_remove(index).1)
        };
        if let Some(locator) = locator {
            let _ = unmark_locator(&locator).await;
        }
    }

    async fn cleanup_markers(&self) {
        let locators = {
            let mut marked = self
                .marked_locators
                .lock()
                .expect("credential marker mutex poisoned");
            marked
                .drain(..)
                .map(|(_, locator)| locator)
                .collect::<Vec<_>>()
        };
        for locator in locators {
            let _ = tokio::time::timeout(Duration::from_secs(2), unmark_locator(&locator)).await;
        }
    }

    fn locator_from_arg(&self, value: &Value) -> Result<Locator, DispatchError> {
        self.registry
            .lock()
            .expect("registry mutex poisoned")
            .locator_arg(value)
    }

    fn call_arg(&self, args: &Value, index: usize) -> Result<CallArg, DispatchError> {
        let Some(value) = args.as_array().and_then(|values| values.get(index)) else {
            return Ok(CallArg::Undefined);
        };
        let Some(reference) = value.get("__sw_ref_arg").and_then(Value::as_u64) else {
            return Ok(CallArg::Value(value.clone().into()));
        };
        match self
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .get(reference)?
        {
            SteadwrightObject::JsHandle(handle) => Ok(CallArg::Handle(handle)),
            SteadwrightObject::ElementHandle(handle) => Ok(CallArg::Handle(handle.0)),
            _ => Err(DispatchError::ordinary(
                "evaluate arguments may only reference JSHandle or ElementHandle objects",
            )),
        }
    }

    async fn describe_target(&self, page: &Page) -> Result<DescribedTarget, DispatchError> {
        // PENDING(phase 6): Chromium implements this private CDP command in Phase 6.
        let response = self
            .raw
            .send(
                "Stead.describeTarget",
                json!({"targetId": page.main_frame().id()}),
            )
            .await
            .map_err(|_| DispatchError::ordinary(PHASE_6_REQUIRED))?;
        let tab_id = response
            .get("tabId")
            .or_else(|| response.get("tab_id"))
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| DispatchError::ordinary(PHASE_6_REQUIRED))?;
        let frame_token = response
            .get("frameToken")
            .or_else(|| response.get("frame_token"))
            .and_then(Value::as_str)
            .ok_or_else(|| DispatchError::ordinary(PHASE_6_REQUIRED))?
            .to_string();
        Ok(DescribedTarget {
            tab_id,
            frame_token,
        })
    }
}

struct DescribedTarget {
    tab_id: i32,
    frame_token: String,
}

/// The browser identifies a credential by the opaque `handle` returned from
/// `stead.credentials.list()`. Accept the whole credential object or the
/// bare handle string.
fn credential_handle(credential: &Value) -> Result<String, DispatchError> {
    if let Some(handle) = credential.as_str() {
        return Ok(handle.to_owned());
    }
    credential
        .get("handle")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            DispatchError::ordinary(
                "stead.credentials: pass a credential from stead.credentials.list() or its handle",
            )
        })
}

async fn mark_locator(locator: &Locator) -> Result<String, DispatchError> {
    let value = locator.evaluate("el => { const n = 'sw-' + Math.random().toString(36).slice(2); el.setAttribute('data-stead-cred', n); return n }", Value::Null).await?;
    match value {
        steadwright::JsValue::String(value) => Ok(value),
        _ => Err(DispatchError::ordinary("Could not mark credential field")),
    }
}

async fn unmark_locator(locator: &Locator) -> Result<(), DispatchError> {
    locator
        .evaluate(
            "el => { el.removeAttribute('data-stead-cred') }",
            Value::Null,
        )
        .await?;
    Ok(())
}

fn bridge_content(result: stead_brain_protocol::ToolResultPayload) -> Result<Value, DispatchError> {
    if result.ok {
        Ok(result.content)
    } else {
        Err(DispatchError::ordinary(result.error.unwrap_or_else(|| {
            "Browser credential operation failed".into()
        })))
    }
}

pub(crate) struct BrowserRuntimePool {
    runtimes: Mutex<HashMap<String, Arc<BrowserJsRuntime>>>,
}

impl Default for BrowserRuntimePool {
    fn default() -> Self {
        Self {
            runtimes: Mutex::new(HashMap::new()),
        }
    }
}

impl BrowserRuntimePool {
    async fn runtime_for(&self, session_id: &str) -> Result<Arc<BrowserJsRuntime>, String> {
        if let Some(runtime) = self.runtimes.lock().await.get(session_id).cloned() {
            return Ok(runtime);
        }
        let runtime = Arc::new(BrowserJsRuntime::new().await?);
        let mut runtimes = self.runtimes.lock().await;
        Ok(runtimes
            .entry(session_id.to_owned())
            .or_insert_with(|| runtime.clone())
            .clone())
    }

    #[allow(dead_code)]
    pub(crate) async fn clear_session(&self, session_id: &str) {
        self.runtimes.lock().await.remove(session_id);
    }
}

struct BrowserJsRuntime {
    runtime: AsyncRuntime,
    registry: Arc<StdMutex<Registry>>,
    state: StdMutex<Value>,
    execution_lock: Mutex<()>,
}

struct ExecutionProgress {
    tool_call_id: String,
    event_sink: Option<BrowserEventSink>,
}

impl BrowserJsRuntime {
    async fn new() -> Result<Self, String> {
        let runtime = AsyncRuntime::new().map_err(|e| e.to_string())?;
        runtime.set_memory_limit(MEMORY_LIMIT).await;
        runtime.set_max_stack_size(STACK_LIMIT).await;
        Ok(Self {
            runtime,
            registry: Arc::new(StdMutex::new(Registry::default())),
            state: StdMutex::new(json!({})),
            execution_lock: Mutex::new(()),
        })
    }

    async fn execute(
        &self,
        code: &str,
        browser: Arc<BrowserConnection>,
        attached: &[TabContext],
        bridge: Arc<dyn BrowserToolBridge>,
        progress: ExecutionProgress,
        cancel: CancellationToken,
    ) -> Result<ExecutionOutcome, JsFailure> {
        let _execution = self.execution_lock.lock().await;
        let js_context = AsyncContext::full(&self.runtime)
            .await
            .map_err(|error| JsFailure::simple(error.to_string()))?;
        let state = self.state.lock().expect("state mutex poisoned").clone();
        let (previous_page, previous_context) = {
            let registry = self.registry.lock().expect("registry mutex poisoned");
            (
                registry.current_page.clone(),
                registry.current_context.clone(),
            )
        };
        let (browser_context, page) = select_context_page(
            browser.browser.as_ref(),
            attached,
            previous_context.as_ref(),
            previous_page.as_ref(),
        )
        .await;
        let roots = self
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .roots_for_execution(browser.browser.clone(), browser_context, page);
        let logs = Arc::new(StdMutex::new(Vec::new()));
        let images = Arc::new(StdMutex::new(Vec::new()));
        let omitted_images = Arc::new(StdMutex::new(0));
        let execution_cancel = cancel.child_token();
        let watchdog_cancel = execution_cancel.clone();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(EXECUTION_TIMEOUT).await;
            watchdog_cancel.cancel();
        });
        let interrupt = execution_cancel.clone();
        self.runtime
            .set_interrupt_handler(Some(Box::new(move || interrupt.is_cancelled())))
            .await;
        let host = Arc::new(ExecutionHost {
            registry: self.registry.clone(),
            bridge,
            raw: browser.raw.clone(),
            tool_call_id: progress.tool_call_id,
            event_sink: progress.event_sink,
            operation_sequence: AtomicU64::new(0),
            credential_sequence: AtomicU64::new(0),
            cancel: execution_cancel.clone(),
            images: images.clone(),
            omitted_images: omitted_images.clone(),
            marked_locators: StdMutex::new(Vec::new()),
        });
        let async_host = host.clone();
        let call = move |receiver: u64, method: String, args: String| {
            let host = async_host.clone();
            async move {
                let envelope = match serde_json::from_str(&args) {
                    Ok(args) => envelope(host.dispatch(receiver, &method, args).await),
                    Err(error) => envelope(Err(DispatchError::ordinary(format!(
                        "Invalid arguments: {error}"
                    )))),
                };
                Ok::<String, rquickjs::Error>(envelope.to_string())
            }
        };
        let sync_host = host.clone();
        let sync_call = move |receiver: u64, method: String, args: String| {
            let result = serde_json::from_str(&args)
                .map_err(|e| DispatchError::ordinary(format!("Invalid arguments: {e}")))
                .and_then(|args| sync_host.dispatch_sync(receiver, &method, args));
            envelope(result).to_string()
        };
        let log_store = logs.clone();
        let log = move |level: String, message: String| {
            log_store
                .lock()
                .expect("log mutex poisoned")
                .push(format!("{level}: {message}"));
        };
        let wrapped = format!(
            r#"(async () => {{
          globalThis.__steadwrightInstall({{"browser":{},"context":{},"page":{}}});
          globalThis.state = globalThis.__steadwrightDecode({state});
          try {{
            const value = await (async () => {{ {code} }})();
            return JSON.stringify({{ok:true,value:globalThis.__steadwrightSerializable(value)}});
          }} catch (error) {{
            return JSON.stringify({{ok:false,name:String(error && error.name || 'Error'),message:String(error && error.message || error),stack:error && error.stack ? String(error.stack) : ''}});
          }}
        }})()"#,
            roots.browser, roots.context, roots.page
        );
        let evaluation = async_with!(js_context => |ctx| {
            ctx.eval::<(), _>(BOOTSTRAP).catch(&ctx).map_err(|e| format!("failed to reset browser_exec globals: {e:?}"))?;
            ctx.globals().set("__steadwright_call", Func::from(Async(call))).catch(&ctx).map_err(|e| format!("failed to bind browser host: {e:?}"))?;
            ctx.globals().set("__steadwright_sync_call", Func::from(sync_call)).catch(&ctx).map_err(|e| format!("failed to bind browser host: {e:?}"))?;
            ctx.globals().set("__steadwright_log", Func::from(log)).catch(&ctx).map_err(|e| format!("failed to bind browser console: {e:?}"))?;
            let promise = ctx.eval::<Promise, _>(wrapped).catch(&ctx).map_err(|e| format!("browser_exec JavaScript error: {e:?}"))?;
            promise.into_future::<String>().await.catch(&ctx).map_err(|e| format!("browser_exec promise failed: {e:?}"))
        });
        let evaluated: Result<String, JsFailure> = tokio::select! {
            result = evaluation => result.map_err(JsFailure::simple),
            _ = execution_cancel.cancelled() => Err(JsFailure::simple(if cancel.is_cancelled() {
                "browser execution cancelled"
            } else {
                "browser execution timed out"
            })),
        };
        host.cleanup_markers().await;
        watchdog.abort();
        self.runtime.set_interrupt_handler(None).await;
        let encoded = evaluated?;
        if let Ok(encoded_state) = async_with!(js_context => |ctx| {
            ctx.eval::<String, _>("JSON.stringify(globalThis.state)")
                .catch(&ctx)
                .map_err(|error| format!("could not persist browser_exec state: {error:?}"))
        })
        .await
        {
            if let Ok(saved) = serde_json::from_str(&encoded_state) {
                *self.state.lock().expect("state mutex poisoned") = saved;
            }
        }
        let parsed: Value = serde_json::from_str(&encoded)
            .map_err(|e| JsFailure::simple(format!("invalid browser_exec result: {e}")))?;
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(JsFailure {
                name: parsed
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Error")
                    .into(),
                message: parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("browser execution failed")
                    .into(),
                stack: parsed
                    .get("stack")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        let images = images.lock().expect("image mutex poisoned").clone();
        let omitted_images = *omitted_images.lock().expect("image mutex poisoned");
        let logs = logs.lock().expect("log mutex poisoned").clone();
        Ok(ExecutionOutcome {
            value: parsed.get("value").cloned().unwrap_or(Value::Null),
            logs,
            images,
            omitted_images,
        })
    }
}

/// Chooses the turn's current page by attached URL, then the session's most
/// recently used page. With neither, `page` remains unresolved until its first
/// `goto`, which creates a page in the selected/default context. Phase 6 can
/// replace URL matching with `Stead.describeTarget` identity.
async fn select_context_page(
    browser: &dyn BrowserApi,
    attached: &[TabContext],
    previous_context: Option<&BrowserContext>,
    previous_page: Option<&Page>,
) -> (BrowserContext, Option<Page>) {
    let contexts = browser.contexts();
    for tab in attached {
        for context in &contexts {
            if !tab.url.is_empty() {
                match context.page_for_url(&tab.url).await {
                    Ok(Some(page)) => return (context.clone(), Some(page)),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            url = %tab.url,
                            error = %error,
                            "attached page remained unusable after initialization retry"
                        );
                    }
                }
            }
        }
    }
    if let Some(page) = previous_page.filter(|page| !page.is_closed()) {
        let context = previous_context
            .cloned()
            .unwrap_or_else(|| browser.default_context());
        return (context, Some(page.clone()));
    }
    (browser.default_context(), None)
}

struct ExecutionOutcome {
    value: Value,
    logs: Vec<String>,
    images: Vec<CapturedImage>,
    omitted_images: usize,
}

struct JsFailure {
    name: String,
    message: String,
    stack: Option<String>,
}

impl JsFailure {
    fn simple(message: impl Into<String>) -> Self {
        Self {
            name: "Error".into(),
            message: message.into(),
            stack: None,
        }
    }

    fn model_message(&self) -> String {
        let mut result = format!("{}: {}", self.name, self.message);
        if let Some(frame) = self
            .stack
            .as_deref()
            .and_then(|stack| stack.lines().skip(1).find(|line| !line.trim().is_empty()))
        {
            result.push('\n');
            result.push_str(frame.trim());
        }
        result
    }
}

fn envelope(result: Result<Value, DispatchError>) -> Value {
    match result {
        Ok(value) => json!({"ok": true, "value": value}),
        Err(error) => {
            json!({"ok": false, "error": error.message, "kind": if error.type_error { "type" } else { "error" }})
        }
    }
}

pub(crate) struct BrowserCodeTool {
    definition: pie_ai::Tool,
    session_id: String,
    bridge: Arc<dyn BrowserToolBridge>,
    attached: Vec<TabContext>,
    runtimes: Arc<BrowserRuntimePool>,
    event_sink: Option<BrowserEventSink>,
}

impl BrowserCodeTool {
    pub(crate) fn new(
        session_id: String,
        bridge: Arc<dyn BrowserToolBridge>,
        attached: Vec<TabContext>,
        runtimes: Arc<BrowserRuntimePool>,
    ) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: "browser_exec".into(),
                description: TOOL_DESCRIPTION.into(),
                parameters: json!({
                    "type":"object", "additionalProperties":false, "required":["code"],
                    "properties":{"code":{"type":"string"}}
                }),
            },
            session_id,
            bridge,
            attached,
            runtimes,
            event_sink: None,
        }
    }

    pub(crate) fn with_event_sink(
        mut self,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
        request_id: String,
    ) -> Self {
        self.event_sink = Some(BrowserEventSink {
            tx,
            session_id: self.session_id.clone(),
            request_id,
        });
        self
    }

    async fn execute_inner(
        &self,
        tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<AgentToolResult, AgentToolError> {
        let code = params
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::Message("browser_exec requires code".into()))?;
        if code.len() > MAX_CODE_BYTES {
            return Err(AgentToolError::Message(format!(
                "browser_exec code exceeds the {MAX_CODE_BYTES}-byte limit"
            )));
        }
        let browser = BrowserHandle::get()
            .await
            .map_err(AgentToolError::Message)?;
        let runtime = self
            .runtimes
            .runtime_for(&self.session_id)
            .await
            .map_err(AgentToolError::Message)?;
        let outcome = runtime
            .execute(
                code,
                browser,
                &self.attached,
                self.bridge.clone(),
                ExecutionProgress {
                    tool_call_id: tool_call_id.to_string(),
                    event_sink: self.event_sink.clone(),
                },
                cancel,
            )
            .await
            .map_err(|e| AgentToolError::Message(e.model_message()))?;
        let result = bounded_value(outcome.value, MAX_RESULT_BYTES);
        let image_note = (outcome.omitted_images > 0).then(|| {
            format!(
                "…[{} screenshot(s) not attached; maximum is {MAX_IMAGES}]",
                outcome.omitted_images
            )
        });
        let logs = bounded_logs(outcome.logs, MAX_LOG_BYTES, image_note);
        let summary = json!({"result": result, "logs": logs});
        let mut content = vec![pie_ai::UserContentBlock::text(summary.to_string())];
        for image in outcome.images {
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
    ) -> Result<AgentToolResult, AgentToolError> {
        if let Some(sink) = &self.event_sink {
            sink.emit(tool_call_id, "running", "browser_exec");
        }
        let result = self.execute_inner(tool_call_id, params, cancel).await;
        if let Some(sink) = &self.event_sink {
            sink.emit(
                tool_call_id,
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                },
                "browser_exec",
            );
        }
        result
    }
}

#[derive(Clone, Copy)]
enum OperationTarget<'a> {
    Credentials,
    Browser,
    Context,
    Page,
    Frame,
    Locator(&'a str),
    FrameLocator,
    JsHandle,
    ElementHandle,
    Response,
    Dialog,
    FileChooser,
    Download,
    Keyboard,
    Mouse,
}

fn operation_target(receiver: Option<&SteadwrightObject>) -> OperationTarget<'_> {
    match receiver {
        None => OperationTarget::Credentials,
        Some(SteadwrightObject::Browser(_)) => OperationTarget::Browser,
        Some(SteadwrightObject::Context(_)) => OperationTarget::Context,
        Some(SteadwrightObject::Page(_)) => OperationTarget::Page,
        Some(SteadwrightObject::Frame(_)) => OperationTarget::Frame,
        Some(SteadwrightObject::Locator(locator)) => OperationTarget::Locator(&locator.description),
        Some(SteadwrightObject::FrameLocator(_)) => OperationTarget::FrameLocator,
        Some(SteadwrightObject::JsHandle(_)) => OperationTarget::JsHandle,
        Some(SteadwrightObject::ElementHandle(_)) => OperationTarget::ElementHandle,
        Some(SteadwrightObject::Response(_)) => OperationTarget::Response,
        Some(SteadwrightObject::Dialog(_)) => OperationTarget::Dialog,
        Some(SteadwrightObject::FileChooser(_)) => OperationTarget::FileChooser,
        Some(SteadwrightObject::Download(_)) => OperationTarget::Download,
        Some(SteadwrightObject::Keyboard(_)) => OperationTarget::Keyboard,
        Some(SteadwrightObject::Mouse(_)) => OperationTarget::Mouse,
    }
}

fn operation_message(target: OperationTarget<'_>, method: &str, args: &Value) -> Option<String> {
    let message = match target {
        OperationTarget::Credentials => match method {
            "credentials.list" => "credentials list".to_string(),
            "credentials.fill" | "credentials.fillTotp" => "credentials fill".to_string(),
            _ => return None,
        },
        OperationTarget::Browser => match method {
            "newPage" => "newPage".to_string(),
            "close" => "close".to_string(),
            _ => return None,
        },
        OperationTarget::Context => match method {
            "newPage" => "newPage".to_string(),
            _ => return None,
        },
        OperationTarget::Page => match method {
            "goto" => argument_message("goto", args, 0),
            "reload" => "reload".to_string(),
            "goBack" => "back".to_string(),
            "goForward" => "forward".to_string(),
            "bringToFront" => "bring to front".to_string(),
            "close" => "close".to_string(),
            "evaluate" | "evaluateHandle" => "evaluate".to_string(),
            "ariaSnapshot" => "ariaSnapshot".to_string(),
            "screenshot" => "screenshot".to_string(),
            "waitForTimeout" => match arg(args, 0).and_then(Value::as_u64) {
                Some(timeout) => format!("wait {timeout}ms"),
                None => "wait".to_string(),
            },
            "waitForFunction" => "waitForFunction".to_string(),
            "waitForSelector" => argument_message("waitForSelector", args, 0),
            "waitForLoadState" => format!(
                "waitForLoadState {}",
                arg(args, 0).and_then(Value::as_str).unwrap_or("load")
            ),
            "waitForURL" => argument_message("waitForURL", args, 0),
            "waitForResponse" => argument_message("waitForResponse", args, 0),
            "waitForRequest" => argument_message("waitForRequest", args, 0),
            "waitForEvent" => argument_message("waitForEvent", args, 0),
            "setViewportSize" => "set viewport size".to_string(),
            _ => return None,
        },
        OperationTarget::Frame => match method {
            "goto" => argument_message("goto", args, 0),
            "evaluate" | "evaluateHandle" => "evaluate".to_string(),
            "ariaSnapshot" => "ariaSnapshot".to_string(),
            "waitForFunction" => "waitForFunction".to_string(),
            "waitForSelector" => argument_message("waitForSelector", args, 0),
            _ => return None,
        },
        OperationTarget::Locator(description) => match method {
            "click"
            | "dblclick"
            | "hover"
            | "tap"
            | "dragTo"
            | "fill"
            | "type"
            | "clear"
            | "check"
            | "uncheck"
            | "setChecked"
            | "setInputFiles"
            | "focus"
            | "blur"
            | "selectText"
            | "dispatchEvent"
            | "scrollIntoViewIfNeeded"
            | "highlight" => format!("{} {}", locator_operation_name(method), description),
            "press" => argument_message("press", args, 0),
            "selectOption" => "select option".to_string(),
            "waitFor" => format!("waitFor {description}"),
            "ariaSnapshot" => "ariaSnapshot".to_string(),
            "screenshot" => "screenshot".to_string(),
            "evaluate" | "evaluateHandle" => "evaluate".to_string(),
            "evaluateAll" => "evaluateAll".to_string(),
            _ => return None,
        },
        OperationTarget::JsHandle => match method {
            "evaluate" | "evaluateHandle" => "evaluate".to_string(),
            _ => return None,
        },
        OperationTarget::ElementHandle => match method {
            "screenshot" => "screenshot".to_string(),
            "evaluate" | "evaluateHandle" => "evaluate".to_string(),
            _ => return None,
        },
        OperationTarget::Dialog => match method {
            "accept" => "accept dialog".to_string(),
            "dismiss" => "dismiss dialog".to_string(),
            _ => return None,
        },
        OperationTarget::FileChooser => match method {
            "setFiles" => "set input files".to_string(),
            _ => return None,
        },
        OperationTarget::Keyboard => match method {
            "press" => argument_message("press", args, 0),
            "type" => "type".to_string(),
            "insertText" => "insert text".to_string(),
            "down" | "up" => argument_message(method, args, 0),
            _ => return None,
        },
        OperationTarget::Mouse => match method {
            "move" => "move mouse".to_string(),
            "click" => "click".to_string(),
            "dblclick" => "dblclick".to_string(),
            "down" => "mouse down".to_string(),
            "up" => "mouse up".to_string(),
            "wheel" => "scroll".to_string(),
            _ => return None,
        },
        OperationTarget::FrameLocator | OperationTarget::Response | OperationTarget::Download => {
            return None;
        }
    };
    Some(truncate_message(&message, 80))
}

fn locator_operation_name(method: &str) -> &str {
    match method {
        "setChecked" => "set checked",
        "setInputFiles" => "set input files",
        "selectText" => "select text",
        "dispatchEvent" => "dispatch event",
        "scrollIntoViewIfNeeded" => "scroll into view",
        _ => method,
    }
}

fn argument_message(prefix: &str, args: &Value, index: usize) -> String {
    match arg(args, index) {
        Some(Value::String(value)) => format!("{prefix} {value}"),
        Some(value) => format!("{prefix} {}", js_value(value)),
        None => prefix.to_string(),
    }
}

fn truncate_message(message: &str, max_chars: usize) -> String {
    if message.chars().count() <= max_chars {
        return message.to_string();
    }
    let mut truncated = message
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn locator_method_description(
    receiver: &SteadwrightObject,
    method: &str,
    args: &Value,
) -> Result<String, DispatchError> {
    let call = method_call_description(method, args)?;
    Ok(match receiver {
        SteadwrightObject::Locator(locator) => format!("{}.{}", locator.description, call),
        SteadwrightObject::FrameLocator(locator) => {
            format!("{}.{}", locator.description, call)
        }
        _ => call,
    })
}

fn method_call_description(method: &str, args: &Value) -> Result<String, DispatchError> {
    let values = args
        .as_array()
        .ok_or_else(|| DispatchError::ordinary("Browser method arguments must be an array"))?;
    Ok(format!(
        "{method}({})",
        values.iter().map(js_value).collect::<Vec<_>>().join(", ")
    ))
}

fn js_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("'{escaped}'")
}

fn js_value(value: &Value) -> String {
    if let Some(regex) = value.get("__sw_regex").and_then(Value::as_str) {
        let flags = value
            .get("__sw_flags")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return format!("/{regex}/{flags}");
    }
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => js_string(value),
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(js_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(values) => format!(
            "{{ {} }}",
            values
                .iter()
                .map(|(key, value)| format!("{key}: {}", js_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn bounded_value(value: Value, limit: usize) -> Value {
    let encoded = serde_json::to_string(&value).expect("JSON value serializes");
    if encoded.len() <= limit {
        return value;
    }
    Value::String(fit_json_string(&encoded, limit).unwrap_or_default())
}

fn fit_json_string(source: &str, limit: usize) -> Option<String> {
    let mut end = floor_char_boundary(source, limit);
    loop {
        let value = format!(
            "{}…[truncated {} bytes]",
            &source[..end],
            source.len() - end
        );
        let serialized_len = serde_json::to_string(&value)
            .expect("string serializes")
            .len();
        if serialized_len <= limit {
            return Some(value);
        }
        if end == 0 {
            return None;
        }
        end = floor_char_boundary(source, end.saturating_sub((serialized_len - limit).max(1)));
    }
}

fn bounded_logs(logs: Vec<String>, limit: usize, required_note: Option<String>) -> Vec<String> {
    let mut result = Vec::new();
    for (index, log) in logs.iter().enumerate() {
        let mut candidate = result.clone();
        candidate.push(log.clone());
        if let Some(note) = &required_note {
            candidate.push(note.clone());
        }
        if serde_json::to_string(&candidate)
            .expect("logs serialize")
            .len()
            <= limit
        {
            result.push(log.clone());
            continue;
        }

        let remainder = logs[index..].join("\n");
        let mut end = floor_char_boundary(&remainder, limit);
        loop {
            let preview = format!(
                "{}…[truncated {} bytes]",
                &remainder[..end],
                remainder.len() - end
            );
            let mut candidate = result.clone();
            candidate.push(preview.clone());
            if let Some(note) = &required_note {
                candidate.push(note.clone());
            }
            let serialized_len = serde_json::to_string(&candidate)
                .expect("logs serialize")
                .len();
            if serialized_len <= limit {
                result.push(preview);
                break;
            }
            if end == 0 {
                break;
            }
            end = floor_char_boundary(
                &remainder,
                end.saturating_sub((serialized_len - limit).max(1)),
            );
        }
        break;
    }
    if let Some(note) = required_note {
        result.push(note);
    }
    debug_assert!(serde_json::to_string(&result).unwrap().len() <= limit);
    result
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn arg(args: &Value, index: usize) -> Option<&Value> {
    args.as_array()
        .and_then(|v| v.get(index))
        .filter(|v| !v.is_null())
}
fn arg_required(args: &Value, index: usize) -> Result<&Value, DispatchError> {
    arg(args, index)
        .ok_or_else(|| DispatchError::ordinary(format!("Missing argument {}", index + 1)))
}
fn string_arg(args: &Value, index: usize) -> Result<&str, DispatchError> {
    arg_required(args, index)?
        .as_str()
        .ok_or_else(|| DispatchError::ordinary(format!("Argument {} must be a string", index + 1)))
}
fn optional_string_arg(args: &Value, index: usize) -> Option<String> {
    arg(args, index).and_then(Value::as_str).map(str::to_owned)
}
fn bool_arg(args: &Value, index: usize) -> Result<bool, DispatchError> {
    arg_required(args, index)?
        .as_bool()
        .ok_or_else(|| DispatchError::ordinary(format!("Argument {} must be a boolean", index + 1)))
}
fn u64_arg(args: &Value, index: usize) -> Result<u64, DispatchError> {
    arg_required(args, index)?.as_u64().ok_or_else(|| {
        DispatchError::ordinary(format!("Argument {} must be a positive number", index + 1))
    })
}
fn i32_arg(args: &Value, index: usize) -> Result<i32, DispatchError> {
    arg_required(args, index)?
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| {
            DispatchError::ordinary(format!("Argument {} must be an integer", index + 1))
        })
}
fn f64_arg(args: &Value, index: usize) -> Result<f64, DispatchError> {
    arg_required(args, index)?
        .as_f64()
        .ok_or_else(|| DispatchError::ordinary(format!("Argument {} must be a number", index + 1)))
}
fn json_arg(args: &Value, index: usize) -> Value {
    arg(args, index).cloned().unwrap_or(Value::Null)
}
fn function_arg(args: &Value, index: usize) -> Result<&str, DispatchError> {
    let value = arg_required(args, index)?;
    value
        .get("__sw_function")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
        .ok_or_else(|| {
            DispatchError::ordinary(format!(
                "Argument {} must be a function or string",
                index + 1
            ))
        })
}
fn timeout_from(value: Option<&Value>) -> Option<Duration> {
    value
        .and_then(|v| v.get("timeout").or(Some(v)))
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
}
fn delay_from(value: Option<&Value>) -> Option<Duration> {
    value
        .and_then(|v| v.get("delay").or(Some(v)))
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
}
fn value_u32(value: &Value, key: &str) -> Result<u32, DispatchError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| DispatchError::ordinary(format!("{key} must be a positive integer")))
}

fn load_state(value: Option<&Value>) -> LoadState {
    match value.and_then(Value::as_str).unwrap_or("load") {
        "domcontentloaded" => LoadState::DomContentLoaded,
        "networkidle" => LoadState::NetworkIdle,
        "commit" => LoadState::Commit,
        _ => LoadState::Load,
    }
}
fn dialog_type_name(value: DialogType) -> &'static str {
    match value {
        DialogType::Alert => "alert",
        DialogType::BeforeUnload => "beforeunload",
        DialogType::Confirm => "confirm",
        DialogType::Prompt => "prompt",
    }
}
fn goto_options(value: Option<&Value>) -> GotoOptions {
    GotoOptions {
        wait_until: value
            .and_then(|v| v.get("waitUntil"))
            .map_or(LoadState::Load, |v| load_state(Some(v))),
        timeout: timeout_from(value),
        referer: value
            .and_then(|v| v.get("referer"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}
fn wait_url_options(value: Option<&Value>) -> WaitForUrlOptions {
    WaitForUrlOptions {
        wait_until: value
            .and_then(|v| v.get("waitUntil"))
            .map_or(LoadState::Load, |v| load_state(Some(v))),
        timeout: timeout_from(value),
    }
}
fn wait_function_options(value: Option<&Value>) -> WaitForFunctionOptions {
    let polling = match value.and_then(|v| v.get("polling")) {
        Some(Value::Number(v)) => {
            Polling::Interval(Duration::from_millis(v.as_u64().unwrap_or(100)))
        }
        _ => Polling::Raf,
    };
    WaitForFunctionOptions {
        polling,
        timeout: timeout_from(value),
    }
}
fn wait_options(value: Option<&Value>) -> WaitForOptions {
    let state = match value
        .and_then(|v| v.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("visible")
    {
        "attached" => WaitForSelectorState::Attached,
        "detached" => WaitForSelectorState::Detached,
        "hidden" => WaitForSelectorState::Hidden,
        _ => WaitForSelectorState::Visible,
    };
    WaitForOptions {
        state,
        timeout: timeout_from(value),
    }
}
fn action_options(value: Option<&Value>) -> ActionOptions {
    let mut options = ActionOptions::default();
    if let Some(v) = value {
        options.force = v.get("force").and_then(Value::as_bool).unwrap_or(false);
        options.no_wait_after = v
            .get("noWaitAfter")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        options.trial = v.get("trial").and_then(Value::as_bool).unwrap_or(false);
        options.timeout = timeout_from(Some(v));
        options.delay = delay_from(Some(v));
        options.click_count = v.get("clickCount").and_then(Value::as_u64).unwrap_or(1) as u8;
        options.button = mouse_button(v.get("button"));
        options.position = v.get("position").map(|p| Point {
            x: p.get("x").and_then(Value::as_f64).unwrap_or(0.0),
            y: p.get("y").and_then(Value::as_f64).unwrap_or(0.0),
        });
    }
    options
}
fn click_options(value: Option<&Value>) -> steadwright::ClickOptions {
    let mut options = steadwright::ClickOptions::default();
    if let Some(v) = value {
        options.button = mouse_button(v.get("button"));
        options.click_count = v.get("clickCount").and_then(Value::as_u64).unwrap_or(1) as u8;
        options.delay = delay_from(Some(v));
        options.steps = v.get("steps").and_then(Value::as_u64).unwrap_or(1) as u32;
    }
    options
}
fn mouse_button(value: Option<&Value>) -> MouseButton {
    match value.and_then(Value::as_str).unwrap_or("left") {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        "back" => MouseButton::Back,
        "forward" => MouseButton::Forward,
        _ => MouseButton::Left,
    }
}
fn screenshot_options(value: Option<&Value>) -> ScreenshotOptions {
    let mut options = ScreenshotOptions::default();
    if let Some(v) = value {
        options.full_page = v.get("fullPage").and_then(Value::as_bool).unwrap_or(false);
        options.omit_background = v
            .get("omitBackground")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        options.quality = v
            .get("quality")
            .and_then(Value::as_u64)
            .and_then(|n| u8::try_from(n).ok());
        options.format = if v.get("type").and_then(Value::as_str) == Some("jpeg") {
            ScreenshotFormat::Jpeg
        } else {
            ScreenshotFormat::Png
        };
        options.timeout = timeout_from(Some(v));
    }
    options
}
fn screenshot_mime_type(format: ScreenshotFormat) -> &'static str {
    match format {
        ScreenshotFormat::Png => "image/png",
        ScreenshotFormat::Jpeg => "image/jpeg",
    }
}
fn aria_options(value: Option<&Value>) -> AriaSnapshotOptions {
    let mut options = AriaSnapshotOptions::default();
    if let Some(v) = value {
        options.depth = v
            .get("depth")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());
        options.boxes = v.get("boxes").and_then(Value::as_bool).unwrap_or(false);
        options.selector = v.get("selector").and_then(Value::as_str).map(str::to_owned);
        options.timeout = timeout_from(Some(v));
    }
    options
}
fn text_match(value: &Value) -> Result<TextMatch, DispatchError> {
    if let Some(text) = value.as_str() {
        return Ok(TextMatch::Str(text.to_owned()));
    }
    if let Some(pattern) = value.get("__sw_regex").and_then(Value::as_str) {
        return Ok(TextMatch::Regex(
            pattern.to_owned(),
            value
                .get("__sw_flags")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ));
    }
    Err(DispatchError::ordinary(
        "Expected a string or JavaScript RegExp",
    ))
}

fn by_role_options(value: &Value) -> Result<ByRoleOptions, DispatchError> {
    Ok(ByRoleOptions {
        name: value.get("name").map(text_match).transpose()?,
        exact: value.get("exact").and_then(Value::as_bool).unwrap_or(false),
        checked: value.get("checked").and_then(Value::as_bool),
        disabled: value.get("disabled").and_then(Value::as_bool),
        expanded: value.get("expanded").and_then(Value::as_bool),
        include_hidden: value.get("includeHidden").and_then(Value::as_bool),
        level: value
            .get("level")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        pressed: value.get("pressed").and_then(Value::as_bool),
        selected: value.get("selected").and_then(Value::as_bool),
    })
}
fn locator_filter(registry: &Registry, value: &Value) -> Result<LocatorFilter, DispatchError> {
    Ok(LocatorFilter {
        has_text: value.get("hasText").map(text_match).transpose()?,
        has_not_text: value.get("hasNotText").map(text_match).transpose()?,
        has: value
            .get("has")
            .map(|v| registry.locator_arg(v))
            .transpose()?,
        has_not: value
            .get("hasNot")
            .map(|v| registry.locator_arg(v))
            .transpose()?,
        visible: value.get("visible").and_then(Value::as_bool),
    })
}
fn locator_text_method(
    receiver: &SteadwrightObject,
    method: &str,
    text: TextMatch,
    exact: bool,
) -> Result<Locator, DispatchError> {
    macro_rules! call {
        ($v:expr) => {
            match method {
                "getByText" => $v.get_by_text(text, exact),
                "getByLabel" => $v.get_by_label(text, exact),
                "getByPlaceholder" => $v.get_by_placeholder(text, exact),
                "getByAltText" => $v.get_by_alt_text(text, exact),
                "getByTitle" => $v.get_by_title(text, exact),
                "getByTestId" => $v.get_by_test_id(text),
                _ => unreachable!(),
            }
        };
    }
    Ok(match receiver {
        SteadwrightObject::Page(Some(v)) => call!(v),
        SteadwrightObject::Frame(v) => call!(v),
        SteadwrightObject::Locator(v) => call!(v),
        SteadwrightObject::FrameLocator(v) => call!(v),
        _ => return Err(DispatchError::unknown(receiver, method)),
    })
}
fn select_values(value: &Value) -> Result<Vec<SelectOptionValue>, DispatchError> {
    let values = if let Some(array) = value.as_array() {
        array.clone()
    } else {
        vec![value.clone()]
    };
    values
        .into_iter()
        .map(|value| {
            if let Some(string) = value.as_str() {
                Ok(SelectOptionValue {
                    value: Some(string.to_owned()),
                    ..Default::default()
                })
            } else if value.is_object() {
                Ok(SelectOptionValue {
                    value: value
                        .get("value")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    label: value
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    index: value
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|v| v as usize),
                    element: None,
                })
            } else {
                Err(DispatchError::ordinary(
                    "selectOption expects a string, object, or array",
                ))
            }
        })
        .collect()
}
fn path_args(args: &Value, index: usize) -> Result<Vec<PathBuf>, DispatchError> {
    let value = arg_required(args, index)?;
    let values = value
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![value.clone()]);
    values
        .into_iter()
        .map(|v| {
            v.as_str()
                .map(PathBuf::from)
                .ok_or_else(|| DispatchError::ordinary("File paths must be strings"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    type RecordedBridgeCall = (String, String, Value);

    struct FakeBrowserApi;

    #[async_trait]
    impl BrowserApi for FakeBrowserApi {
        fn contexts(&self) -> Vec<BrowserContext> {
            Vec::new()
        }

        fn default_context(&self) -> BrowserContext {
            panic!("the dispatcher test does not request a default context")
        }

        async fn new_page(&self) -> steadwright::Result<Page> {
            Err(steadwright::Error::InvalidArgument(
                "fake new page failure".into(),
            ))
        }

        async fn close(&self) -> steadwright::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn result_truncation_is_bounded_and_unicode_safe() {
        let value = bounded_value(json!("🦀".repeat(MAX_RESULT_BYTES)), MAX_RESULT_BYTES);
        let text = value.as_str().expect("large values become a preview");
        assert!(serde_json::to_string(&value).unwrap().len() <= MAX_RESULT_BYTES);
        assert!(text.contains("…[truncated "));
    }

    #[test]
    fn result_truncation_accounts_for_json_escaping() {
        let value = bounded_value(json!("\\\"".repeat(MAX_RESULT_BYTES)), MAX_RESULT_BYTES);
        assert!(serde_json::to_string(&value).unwrap().len() <= MAX_RESULT_BYTES);
        assert!(value.as_str().unwrap().contains("…[truncated "));
    }

    #[test]
    fn tool_contract_is_code_only_and_sequential() {
        let tool = BrowserCodeTool::new(
            "test-contract".into(),
            Arc::new(FakeBridge),
            Vec::new(),
            Arc::new(BrowserRuntimePool::default()),
        );
        assert_eq!(tool.definition().description, TOOL_DESCRIPTION);
        assert_eq!(tool.definition().parameters["required"], json!(["code"]));
        assert!(
            tool.definition().parameters["properties"]
                .get("tab_id")
                .is_none()
        );
        assert_eq!(tool.execution_mode(), Some(ToolExecutionMode::Sequential));
    }

    #[test]
    fn logs_are_bounded_and_report_omitted_bytes() {
        let logs = bounded_logs(vec!["\\\"é".repeat(MAX_LOG_BYTES)], MAX_LOG_BYTES, None);
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("…[truncated "));
        assert!(serde_json::to_string(&logs).unwrap().len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn screenshot_omission_note_is_reserved_in_log_budget() {
        let note = "…[2 screenshot(s) not attached; maximum is 4]".to_string();
        let logs = bounded_logs(
            vec!["x".repeat(MAX_LOG_BYTES)],
            MAX_LOG_BYTES,
            Some(note.clone()),
        );
        assert_eq!(logs.last(), Some(&note));
        assert!(serde_json::to_string(&logs).unwrap().len() <= MAX_LOG_BYTES);
    }

    #[test]
    fn javascript_errors_include_name_message_and_first_frame() {
        let failure = JsFailure {
            name: "TypeError".into(),
            message: "page.foo is not a function".into(),
            stack: Some(
                "TypeError: page.foo is not a function\n    at <anonymous>:2\n    at ignored:3"
                    .into(),
            ),
        };
        assert_eq!(
            failure.model_message(),
            "TypeError: page.foo is not a function\nat <anonymous>:2"
        );
    }

    #[test]
    fn screenshots_are_deduplicated_and_limited() {
        let host = test_host();
        host.image_result(vec![1], "image/png").unwrap();
        host.image_result(vec![1], "image/png").unwrap();
        for byte in 2..=6 {
            host.image_result(vec![byte], "image/png").unwrap();
        }
        assert_eq!(host.images.lock().unwrap().len(), MAX_IMAGES);
        assert_eq!(*host.omitted_images.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn dispatcher_uses_the_browser_api_seam() {
        let host = test_host();
        let receiver = host
            .registry
            .lock()
            .unwrap()
            .insert(SteadwrightObject::Browser(Arc::new(FakeBrowserApi)));
        assert_eq!(
            host.dispatch_sync(receiver, "contexts", json!([])).unwrap(),
            json!([])
        );
        let error = host
            .dispatch(receiver, "newPage", json!([]))
            .await
            .unwrap_err();
        assert!(error.message.contains("fake new page failure"));
    }

    #[tokio::test]
    async fn browser_script_emits_parent_and_operation_tool_statuses() {
        #[derive(Clone)]
        struct FakeScriptBrowserApi {
            sink: BrowserEventSink,
            sequence: Arc<AtomicU64>,
            locator_description: Arc<StdMutex<Option<String>>>,
        }

        impl FakeScriptBrowserApi {
            fn dispatch_sync(&self, receiver: u64, method: &str, args: &str) -> String {
                let result = (|| {
                    if receiver != 1 || method != "getByRole" {
                        return Err(DispatchError::ordinary("unexpected fake sync call"));
                    }
                    let args = serde_json::from_str::<Value>(args)
                        .map_err(|error| DispatchError::ordinary(error.to_string()))?;
                    *self.locator_description.lock().unwrap() =
                        Some(method_call_description(method, &args)?);
                    Ok(json!({"__sw_ref": 2, "__sw_type": "Locator"}))
                })();
                envelope(result).to_string()
            }

            async fn dispatch(&self, receiver: u64, method: &str, args: &str) -> String {
                let result = (|| {
                    let args = serde_json::from_str::<Value>(args)
                        .map_err(|error| DispatchError::ordinary(error.to_string()))?;
                    let locator_description = self.locator_description.lock().unwrap().clone();
                    let target = match receiver {
                        1 => OperationTarget::Page,
                        2 => OperationTarget::Locator(locator_description.as_deref().ok_or_else(
                            || DispatchError::ordinary("fake locator was not constructed"),
                        )?),
                        _ => return Err(DispatchError::ordinary("unexpected fake receiver")),
                    };
                    let message = operation_message(target, method, &args)
                        .ok_or_else(|| DispatchError::ordinary("unexpected fake operation"))?;
                    let progress = OperationProgress::start(
                        &self.sink,
                        "parent-call",
                        self.sequence.fetch_add(1, Ordering::Relaxed),
                        message,
                    );
                    let result = match (receiver, method) {
                        (1, "goto") | (2, "click") => Ok(Value::Null),
                        _ => Err(DispatchError::ordinary("unexpected fake operation")),
                    };
                    progress.finish(result.is_ok());
                    result
                })();
                envelope(result).to_string()
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = BrowserEventSink {
            tx,
            session_id: "session-a".into(),
            request_id: "request-a".into(),
        };
        let api = FakeScriptBrowserApi {
            sink: sink.clone(),
            sequence: Arc::new(AtomicU64::new(0)),
            locator_description: Arc::new(StdMutex::new(None)),
        };
        sink.emit("parent-call", "running", "browser_exec");

        let runtime = AsyncRuntime::new().unwrap();
        let context = AsyncContext::full(&runtime).await.unwrap();
        let async_api = api.clone();
        let call = move |receiver: u64, method: String, args: String| {
            let api = async_api.clone();
            async move { Ok::<String, rquickjs::Error>(api.dispatch(receiver, &method, &args).await) }
        };
        let sync_api = api.clone();
        let sync_call = move |receiver: u64, method: String, args: String| {
            sync_api.dispatch_sync(receiver, &method, &args)
        };
        let result = async_with!(context => |ctx| {
            ctx.eval::<(), _>(BOOTSTRAP).unwrap();
            ctx.globals()
                .set("__steadwright_call", Func::from(Async(call)))
                .unwrap();
            ctx.globals()
                .set("__steadwright_sync_call", Func::from(sync_call))
                .unwrap();
            ctx.globals()
                .set("__steadwright_log", Func::from(|_: String, _: String| {}))
                .unwrap();
            ctx.eval::<(), _>("__steadwrightInstall({browser:3,context:4,page:1})")
                .unwrap();
            let promise = ctx
                .eval::<Promise, _>(
                    "(async () => { const u = 'https://www.apple.com/ca/store'; await page.goto(u); await page.getByRole('button', {name:'Go'}).click(); return 1 })()",
                )
                .unwrap();
            promise.into_future::<i32>().await.unwrap()
        })
        .await;
        assert_eq!(result, 1);
        sink.emit("parent-call", "completed", "browser_exec");

        let statuses = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| {
                assert_eq!(event.request_id.as_deref(), Some("request-a"));
                assert_eq!(event.session_id.as_deref(), Some("session-a"));
                match event.event {
                    BrainEvent::ToolStatus(status) => status,
                    event => panic!("unexpected event: {event:?}"),
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                ToolStatus {
                    tool_call_id: "parent-call".into(),
                    status: "running".into(),
                    message: Some("browser_exec".into()),
                },
                ToolStatus {
                    tool_call_id: "parent-call:op:0".into(),
                    status: "running".into(),
                    message: Some("goto https://www.apple.com/ca/store".into()),
                },
                ToolStatus {
                    tool_call_id: "parent-call:op:0".into(),
                    status: "completed".into(),
                    message: Some("goto https://www.apple.com/ca/store".into()),
                },
                ToolStatus {
                    tool_call_id: "parent-call:op:1".into(),
                    status: "running".into(),
                    message: Some("click getByRole('button', { name: 'Go' })".into()),
                },
                ToolStatus {
                    tool_call_id: "parent-call:op:1".into(),
                    status: "completed".into(),
                    message: Some("click getByRole('button', { name: 'Go' })".into()),
                },
                ToolStatus {
                    tool_call_id: "parent-call".into(),
                    status: "completed".into(),
                    message: Some("browser_exec".into()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn credential_bridge_preserves_marker_payloads_and_unique_ids() {
        let bridge = Arc::new(RecordingBridge::default());
        let host = test_host_with_bridge(bridge.clone());
        let arguments = json!({
            "tab_id": 7,
            "frame_token": "frame-a",
            "credential": "credential-1",
            "marker": "sw-pass"
        });
        host.call_credential_bridge("fill", "browser.fill_credential", arguments.clone())
            .await
            .unwrap();
        host.call_credential_bridge("fill", "browser.fill_credential", arguments.clone())
            .await
            .unwrap();

        let calls = bridge.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, "browser.fill_credential");
        assert_eq!(calls[0].2, arguments);
        assert_ne!(calls[0].0, calls[1].0);
    }

    struct FakeBridge;
    #[async_trait]
    impl BrowserToolBridge for FakeBridge {
        async fn call_browser_tool(
            &self,
            _id: &str,
            _name: &str,
            _arguments: Value,
            _cancel: CancellationToken,
        ) -> Result<stead_brain_protocol::ToolResultPayload, super::super::BrainError> {
            unreachable!()
        }
    }

    #[derive(Default)]
    struct RecordingBridge {
        calls: StdMutex<Vec<RecordedBridgeCall>>,
    }

    #[async_trait]
    impl BrowserToolBridge for RecordingBridge {
        async fn call_browser_tool(
            &self,
            id: &str,
            name: &str,
            arguments: Value,
            _cancel: CancellationToken,
        ) -> Result<stead_brain_protocol::ToolResultPayload, super::super::BrainError> {
            self.calls
                .lock()
                .unwrap()
                .push((id.to_string(), name.to_string(), arguments));
            Ok(stead_brain_protocol::ToolResultPayload {
                ok: true,
                content: Value::Null,
                error: None,
                tainted: true,
            })
        }
    }

    fn test_host() -> ExecutionHost {
        test_host_with_bridge(Arc::new(FakeBridge))
    }

    fn test_host_with_bridge(bridge: Arc<dyn BrowserToolBridge>) -> ExecutionHost {
        let (tx, _rx) = mpsc::channel(1);
        struct DeadTransport {
            incoming: StdMutex<Option<Incoming>>,
            _tx: mpsc::Sender<Result<String, TransportError>>,
        }
        #[async_trait]
        impl Transport for DeadTransport {
            async fn send(&self, _message: String) -> Result<(), TransportError> {
                Err(TransportError::Closed)
            }
            fn incoming(&mut self) -> Incoming {
                self.incoming.lock().unwrap().take().unwrap()
            }
            async fn close(&self) {}
        }
        ExecutionHost {
            registry: Arc::new(StdMutex::new(Registry::default())),
            bridge,
            raw: RawCdp {
                transport: Arc::new(Mutex::new(Box::new(DeadTransport {
                    incoming: StdMutex::new(Some(_rx)),
                    _tx: tx,
                }))),
                pending: Arc::new(StdMutex::new(HashMap::new())),
                next_id: Arc::new(AtomicU64::new(RAW_CDP_ID_BASE)),
            },
            tool_call_id: "test".into(),
            event_sink: None,
            operation_sequence: AtomicU64::new(0),
            credential_sequence: AtomicU64::new(0),
            cancel: CancellationToken::new(),
            images: Arc::new(StdMutex::new(Vec::new())),
            omitted_images: Arc::new(StdMutex::new(0)),
            marked_locators: StdMutex::new(Vec::new()),
        }
    }
}

#[cfg(test)]
mod credential_handle_tests {
    use super::credential_handle;
    use serde_json::json;

    #[test]
    fn accepts_credential_objects_and_bare_handles() {
        assert_eq!(
            credential_handle(&json!({"handle": "h1", "label": "a@b"})).unwrap(),
            "h1"
        );
        assert_eq!(credential_handle(&json!("h2")).unwrap(), "h2");
        assert!(credential_handle(&json!({"id": "x"})).is_err());
    }
}
