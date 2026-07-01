use std::collections::{BTreeMap, HashMap};
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Local, Utc};
use pie_agent_core::{
    AgentEvent, AgentHarness, AgentHarnessOptions, AgentMessage, AgentTool, AgentToolError,
    AgentToolResult, AgentToolUpdate, MemorySessionStorage, NativeEnv, PermissionClassification,
    Session, SessionStorage, Skill, SkillSource, ThinkingLevel, ToolExecutionMode,
    format_skill_invocation, load_skills,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use stead_brain_protocol::{
    AssistantDone, BrainEvent, CreateSessionParams, ErrorInfo, InitializeParams, ModelCatalogEntry,
    ModelCatalogProvider, NotificationInfo, PROTOCOL_VERSION, ReadyInfo, ResponseEnvelope,
    SendMessageParams, SessionInfo, ToolCallEnvelope, ToolResultEnvelope, ToolResultPayload,
    ToolStatus, UsageUpdate,
};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use walkdir::WalkDir;

mod auth;

pub use auth::{CredentialAuthType, ProviderAuthStore};

const BRAIN_VERSION: &str = env!("CARGO_PKG_VERSION");
const PIE_PIN: &str = include_str!("../../../PIE_PIN.txt");
const MAX_READ_BYTES: u64 = 512 * 1024;
const MAX_SEARCH_BYTES: u64 = 128 * 1024;
const MAX_SEARCH_MATCHES: usize = 200;
const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;
const MAX_INSTRUCTION_FILE_BYTES: u64 = 64 * 1024;
const MAX_MEMORY_ENTRY_BYTES: usize = 64 * 1024;
const MAX_MEMORY_BLOCK_BYTES: usize = 96 * 1024;
const MAX_MEMORY_SEARCH_MATCHES: usize = 64;
const MAX_MEMORY_ENTRIES: usize = 256;
const MAX_MEMORY_NAME_CHARS: usize = 120;
const MAX_SKILL_CONTENT_CHARS: usize = 96 * 1024;
const MAX_SKILLS: usize = 64;
const WEB_FETCH_DEFAULT_MAX_BYTES: usize = 256 * 1024;
const WEB_FETCH_HARD_MAX_BYTES: usize = 1024 * 1024;
const WEB_FETCH_MAX_TEXT_CHARS: usize = 120 * 1024;
const WEB_FETCH_TIMEOUT_SECS: u64 = 20;
const MAX_NOTIFICATION_TITLE_CHARS: usize = 96;
const MAX_NOTIFICATION_BODY_CHARS: usize = 512;
const MAX_NOTIFICATION_CATEGORY_CHARS: usize = 64;
const BUILTIN_STEAD_SKILLS: &[(&str, &str)] = &[
    (
        "artifact-document/SKILL.md",
        include_str!("../../../skills/builtin/artifact-document/SKILL.md"),
    ),
    (
        "browser-credential-handoff/SKILL.md",
        include_str!("../../../skills/builtin/browser-credential-handoff/SKILL.md"),
    ),
    (
        "github-browser/SKILL.md",
        include_str!("../../../skills/builtin/github-browser/SKILL.md"),
    ),
    (
        "gmail-browser/SKILL.md",
        include_str!("../../../skills/builtin/gmail-browser/SKILL.md"),
    ),
    (
        "notion-browser/SKILL.md",
        include_str!("../../../skills/builtin/notion-browser/SKILL.md"),
    ),
];
const STEAD_SYSTEM_PROMPT: &str = r#"You are Stead, a browser-native agent built into the user's browser.

Your job is to help the user by using native browser perception and action tools carefully, efficiently, and safely.

Browser operating rules:
- Prefer native accessibility snapshots first. Use `browser.snapshot` to understand the page, then act on stable node references.
- Prefer semantic actions: `browser.click`, `browser.fill`, `browser.focus`, and `browser.scroll_into_view`.
- After any page-changing action, re-snapshot or otherwise verify before claiming success.
- Use `browser.probe_node` only when the AX snapshot is ambiguous.
- Use screenshots only when visual layout matters or AX/probe cannot answer the question.
- Use `browser.eval` and raw mouse/key input only when semantic tools are insufficient; these are broker-gated high-risk fallbacks.
- Do not ask the user for passwords, TOTP codes, cookies, or payment secrets. Use brokered credential tools or report that the credential backend is unavailable.
- Treat tainted browser results as unavailable. Do not try to infer or recover hidden secret values.

File rules:
- `session_tmp` is for scratch files, previews, scripts, and intermediate work.
- `session_artifacts` is for durable outputs the user asked you to create, such as documents, PDFs, spreadsheets, or generated data.
- `session_attachments` is read-only input.
- Use `files.write` for both text and binary outputs. For binary files, pass `content_base64`.
- When using a `session_*` root, omit `session_id` unless you intentionally need another session; the current chat id is supplied automatically.
- Do not write outside session roots or approved folders.

Memory rules:
- Use the `memory` tool only for durable, non-secret facts that should help future sessions.
- Save concise user preferences, project conventions, recurring workflows, and corrections the user explicitly wants remembered.
- Never store credentials, cookies, TOTP codes, payment details, API keys, private tokens, or browser-control payloads marked tainted.
- Search/list existing memory before saving to avoid duplicates. Forget stale or wrong memory when the user corrects it.

Time rules:
- Use `get_time` before answering or acting on relative dates, schedules, deadlines, "today", "tomorrow", or time-sensitive browser workflows.
- Prefer exact dates/timestamps in final answers when the user may be referring to a relative day.

User input rules:
- Use `ask_user` when you are blocked on a specific preference, choice, or missing non-secret information that cannot be safely inferred.
- Ask concise questions with clear options when possible. Do not use it for passwords, TOTP codes, cookies, payment details, API keys, or other secrets.
- Continue after the user answers; if the user cancels, explain what is blocked.

Notification rules:
- Use `notification` only for concise user-visible milestones, completion notices, or blocked-state notices.
- Do not put secrets, credentials, cookies, TOTP codes, payment details, API keys, or tainted browser payloads in notifications.

Web fetch rules:
- Use `WebFetch` for public, credentialless HTTP(S) reads when browser cookies, page state, or the current logged-in session are not needed.
- Do not use `WebFetch` for logged-in pages, local secrets, browser state, or anything requiring the user's authenticated tab context; use browser tools instead.
- Keep fetched content compact and cite the fetched URL when it materially informs the answer.

Behavior:
- Be direct and concise in chat.
- When you need to use tools, explain progress briefly only when useful.
- Keep tool results compact. Avoid expensive screenshots, broad file searches, and repeated full-page snapshots when a narrower read is enough.
- If blocked by policy, missing credentials, missing browser context, or unavailable tooling, say exactly what is blocked and what would unblock it."#;

#[derive(Debug, Error)]
pub enum BrainError {
    #[error("brain has not been initialized")]
    Uninitialized,
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("file access denied: {0}")]
    FileAccessDenied(String),
    #[error("model not configured")]
    ModelNotConfigured,
    #[error("model not found: {provider}/{model}")]
    ModelNotFound { provider: String, model: String },
    #[error("agent run failed: {0}")]
    AgentRun(String),
    #[error("provider auth failed: {0}")]
    ProviderAuth(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, BrainError>;

#[derive(Clone, Debug)]
pub struct BrainConfig {
    pub app_support_dir: PathBuf,
    pub approved_roots: Vec<PathBuf>,
    pub dev_allow_config_files: bool,
}

impl BrainConfig {
    pub fn from_initialize(params: InitializeParams) -> Self {
        Self {
            app_support_dir: params
                .app_support_dir
                .unwrap_or_else(default_app_support_dir),
            approved_roots: params.approved_roots,
            dev_allow_config_files: params.dev_allow_config_files,
        }
    }

    pub fn agent_root(&self) -> PathBuf {
        self.app_support_dir.join("agents").join("main")
    }
}

#[derive(Clone)]
pub struct BrainCore {
    config: BrainConfig,
    sessions: SessionStore,
    files: FileAccess,
    memory: MemoryStore,
    pending_tools: PendingToolResults,
    active_turns: ActiveTurns,
    auth: ProviderAuthStore,
}

type PendingToolResults = Arc<Mutex<HashMap<String, oneshot::Sender<ToolResultPayload>>>>;
type ActiveTurns = Arc<Mutex<HashMap<String, ActiveTurn>>>;

#[derive(Clone)]
struct ActiveTurn {
    request_id: String,
    harness: Arc<AgentHarness>,
}

#[async_trait]
pub trait BrowserToolBridge: Send + Sync {
    async fn call_browser_tool(
        &self,
        tool_call_id: &str,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<stead_brain_protocol::ToolResultPayload>;
}

pub fn browser_tools(bridge: Arc<dyn BrowserToolBridge>) -> Vec<Arc<dyn AgentTool>> {
    browser_tool_names()
        .into_iter()
        .map(|name| Arc::new(BrowserMediatedTool::new(name, bridge.clone())) as Arc<dyn AgentTool>)
        .collect()
}

pub fn browser_tool_names() -> Vec<&'static str> {
    vec![
        "browser.list_tabs",
        "browser.snapshot",
        "browser.probe_node",
        "browser.screenshot",
        "browser.click",
        "browser.fill",
        "browser.focus",
        "browser.scroll_into_view",
        "browser.navigate",
        "browser.open_tab",
        "browser.close_tab",
        "browser.eval",
        "browser.key",
        "browser.mouse_click",
        "browser.mouse_move",
        "browser.mouse_down",
        "browser.mouse_up",
        "browser.mouse_drag",
        "browser.scroll",
        "browser.handle_dialog",
        "browser.handle_file_chooser",
        "browser.mark_credential_injection",
        "browser.list_credentials",
        "browser.fill_credential",
        "browser.fill_totp",
    ]
}

pub fn file_tools(files: Arc<FileAccess>) -> Vec<Arc<dyn AgentTool>> {
    file_tools_for_session(files, None)
}

pub fn file_tools_for_session(
    files: Arc<FileAccess>,
    default_session_id: Option<String>,
) -> Vec<Arc<dyn AgentTool>> {
    file_tool_names()
        .into_iter()
        .map(|name| {
            Arc::new(FileTool::new(
                name,
                files.clone(),
                default_session_id.clone(),
            )) as Arc<dyn AgentTool>
        })
        .collect()
}

pub fn file_tool_names() -> Vec<&'static str> {
    vec!["files.list", "files.read", "files.search", "files.write"]
}

pub fn memory_tools(memory: Arc<MemoryStore>) -> Vec<Arc<dyn AgentTool>> {
    vec![Arc::new(MemoryTool::new(memory)) as Arc<dyn AgentTool>]
}

pub fn memory_tool_names() -> Vec<&'static str> {
    vec!["memory"]
}

pub fn user_prompt_tools(
    session_id: String,
    request_id: String,
    pending_tools: PendingToolResults,
    tx: mpsc::UnboundedSender<ResponseEnvelope>,
) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(AskUserTool::new(
            session_id.clone(),
            request_id.clone(),
            pending_tools,
            tx.clone(),
        )) as Arc<dyn AgentTool>,
        Arc::new(NotificationTool::new(session_id, request_id, tx)) as Arc<dyn AgentTool>,
    ]
}

pub fn user_prompt_tool_names() -> Vec<&'static str> {
    vec!["ask_user", "notification"]
}

pub fn local_tools() -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(GetTimeTool::new()) as Arc<dyn AgentTool>,
        Arc::new(WebFetchTool::new()) as Arc<dyn AgentTool>,
    ]
}

pub fn local_tool_names() -> Vec<&'static str> {
    vec!["get_time", "WebFetch"]
}

struct BrowserMediatedTool {
    definition: pie_ai::Tool,
    bridge: Arc<dyn BrowserToolBridge>,
}

impl BrowserMediatedTool {
    fn new(name: &'static str, bridge: Arc<dyn BrowserToolBridge>) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: name.to_string(),
                description: browser_tool_description(name).to_string(),
                parameters: browser_tool_parameters(name),
            },
            bridge,
        }
    }
}

#[async_trait]
impl AgentTool for BrowserMediatedTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    fn permission_classification(&self, _prepared_args: &Value) -> PermissionClassification {
        // Browser-side AgentControl/ControlBroker is the authoritative policy
        // layer; prompting here would create a second, divergent gate.
        PermissionClassification::Allow
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let result = self
            .bridge
            .call_browser_tool(tool_call_id, &self.definition.name, params, cancel)
            .await
            .map_err(|error| AgentToolError::Message(error.to_string()))?;
        if !result.ok {
            return Err(AgentToolError::Message(
                result
                    .error
                    .unwrap_or_else(|| "browser tool failed".to_string()),
            ));
        }
        let (content, details) = browser_tool_result_content(result);
        Ok(AgentToolResult {
            content,
            details,
            terminate: None,
        })
    }
}

fn browser_tool_result_content(
    result: ToolResultPayload,
) -> (Vec<pie_ai::UserContentBlock>, Value) {
    if result.tainted {
        return (
            vec![pie_ai::UserContentBlock::text(
                "[tainted browser tool result withheld]",
            )],
            json!({ "tainted": true }),
        );
    }

    let mut details = result.content;
    let mime_type = details
        .get("mime_type")
        .and_then(Value::as_str)
        .filter(|mime| mime.starts_with("image/"))
        .unwrap_or("image/png")
        .to_string();
    let image_base64 = details.as_object_mut().and_then(|object| {
        object.remove("image_base64").and_then(|value| {
            value.as_str().map(|data| {
                object.insert("image_base64_chars".to_string(), json!(data.len()));
                data.to_string()
            })
        })
    });

    let mut content = vec![pie_ai::UserContentBlock::text(details.to_string())];
    if let Some(data) = image_base64.filter(|data| !data.is_empty()) {
        content.push(pie_ai::UserContentBlock::Image(pie_ai::ImageContent {
            data,
            mime_type,
        }));
    }
    (content, details)
}

struct FileTool {
    definition: pie_ai::Tool,
    files: Arc<FileAccess>,
    default_session_id: Option<String>,
}

impl FileTool {
    fn new(name: &'static str, files: Arc<FileAccess>, default_session_id: Option<String>) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: name.to_string(),
                description: file_tool_description(name).to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": true
                }),
            },
            files,
            default_session_id,
        }
    }

    fn with_default_session_id(&self, mut params: Value) -> Value {
        let needs_default = params
            .get("root")
            .and_then(Value::as_str)
            .and_then(SessionRoot::parse)
            .is_some()
            && params.get("session_id").is_none();
        if needs_default {
            if let (Some(session_id), Some(object)) =
                (self.default_session_id.as_ref(), params.as_object_mut())
            {
                object.insert("session_id".to_string(), Value::String(session_id.clone()));
            }
        }
        params
    }
}

#[async_trait]
impl AgentTool for FileTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let params = self.with_default_session_id(params);
        let details = match self.definition.name.as_str() {
            "files.list" => {
                let target = self.files.target_from_params(&params, "path", true).await?;
                let entries = self.files.list(target).await.map_err(tool_error)?;
                json!({ "entries": entries })
            }
            "files.read" => {
                let target = self
                    .files
                    .target_from_params(&params, "path", false)
                    .await?;
                let contents = self
                    .files
                    .read_to_string(target)
                    .await
                    .map_err(tool_error)?;
                json!({ "content": contents })
            }
            "files.search" => {
                let target = self.files.target_from_params(&params, "root", true).await?;
                let pattern = required_string(&params, "pattern")?;
                let matches = self
                    .files
                    .search(target, pattern)
                    .await
                    .map_err(tool_error)?;
                json!({ "matches": matches })
            }
            "files.write" => {
                let target = self.files.write_target_from_params(&params).await?;
                let content = content_bytes(&params)?;
                let path = self
                    .files
                    .write(target, &content)
                    .await
                    .map_err(tool_error)?;
                json!({ "path": path })
            }
            _ => return Err(AgentToolError::Message("unknown file tool".to_string())),
        };
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(details.to_string())],
            details,
            terminate: None,
        })
    }
}

struct MemoryTool {
    definition: pie_ai::Tool,
    memory: Arc<MemoryStore>,
}

impl MemoryTool {
    fn new(memory: Arc<MemoryStore>) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: "memory".to_string(),
                description: "Persistent cross-session memory under the Stead agent home. Use action=save/list/read/search/forget for durable non-secret preferences, project facts, and corrections only.".to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["save", "list", "read", "search", "forget"]
                        },
                        "name": {
                            "type": "string",
                            "description": "Human-readable memory name for save/read/forget. It is normalized to a safe local key."
                        },
                        "description": {
                            "type": "string",
                            "description": "One-line summary for save."
                        },
                        "type": {
                            "type": "string",
                            "description": "Optional category such as user, project, workflow, correction, preference."
                        },
                        "content": {
                            "type": "string",
                            "description": "Memory body for save."
                        },
                        "query": {
                            "type": "string",
                            "description": "Case-insensitive substring query for search."
                        }
                    }
                }),
            },
            memory,
        }
    }
}

#[async_trait]
impl AgentTool for MemoryTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let action = required_string(&params, "action")?;
        let details = match action {
            "save" => {
                let name = required_string(&params, "name")?;
                let description = required_string(&params, "description")?;
                let content = required_string(&params, "content")?;
                let kind = params.get("type").and_then(Value::as_str).unwrap_or("user");
                let entry = self
                    .memory
                    .save(name, description, kind, content)
                    .await
                    .map_err(tool_error)?;
                json!({ "saved": entry })
            }
            "list" => {
                let entries = self.memory.list().await.map_err(tool_error)?;
                json!({ "memories": entries })
            }
            "read" => {
                let name = required_string(&params, "name")?;
                let entry = self.memory.read(name).await.map_err(tool_error)?;
                json!({ "memory": entry })
            }
            "search" => {
                let query = required_string(&params, "query")?;
                let matches = self.memory.search(query).await.map_err(tool_error)?;
                json!({ "matches": matches })
            }
            "forget" => {
                let name = required_string(&params, "name")?;
                let forgotten = self.memory.forget(name).await.map_err(tool_error)?;
                json!({ "forgotten": forgotten })
            }
            _ => {
                return Err(AgentToolError::Message(format!(
                    "unknown memory action `{action}`"
                )));
            }
        };
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(details.to_string())],
            details,
            terminate: None,
        })
    }
}

struct AskUserTool {
    definition: pie_ai::Tool,
    session_id: String,
    request_id: String,
    pending_tools: PendingToolResults,
    tx: mpsc::UnboundedSender<ResponseEnvelope>,
}

impl AskUserTool {
    fn new(
        session_id: String,
        request_id: String,
        pending_tools: PendingToolResults,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
    ) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: "ask_user".to_string(),
                description: "Ask the user for a concise non-secret decision or missing detail, then wait for their response.".to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["prompt"],
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "Short explanation of what you need from the user."
                        },
                        "questions": {
                            "type": "array",
                            "description": "One or more concise questions. If omitted, prompt is used as a single free-form question.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["id", "question"],
                                "properties": {
                                    "id": {
                                        "type": "string",
                                        "description": "Stable snake_case identifier for this question."
                                    },
                                    "question": { "type": "string" },
                                    "header": {
                                        "type": "string",
                                        "description": "Short category label."
                                    },
                                    "multiple": {
                                        "type": "boolean",
                                        "description": "Whether multiple options may be selected."
                                    },
                                    "options": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "required": ["label"],
                                            "properties": {
                                                "label": { "type": "string" },
                                                "description": { "type": "string" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }),
            },
            session_id,
            request_id,
            pending_tools,
            tx,
        }
    }
}

#[async_trait]
impl AgentTool for AskUserTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let prompt = required_string(&params, "prompt")?.trim();
        if prompt.is_empty() {
            return Err(AgentToolError::Message(
                "`ask_user.prompt` must not be empty".to_string(),
            ));
        }
        let pending_key = pending_tool_key(&self.session_id, tool_call_id);
        let (result_tx, result_rx) = oneshot::channel();
        self.pending_tools
            .lock()
            .await
            .insert(pending_key.clone(), result_tx);

        emit_response(
            &self.tx,
            ResponseEnvelope::session_event(
                Some(self.request_id.clone()),
                self.session_id.clone(),
                BrainEvent::ToolStatus(ToolStatus {
                    tool_call_id: tool_call_id.to_string(),
                    status: "waiting_for_user".to_string(),
                    message: Some(prompt.to_string()),
                }),
            ),
        );
        emit_response(
            &self.tx,
            ResponseEnvelope::session_event(
                Some(self.request_id.clone()),
                self.session_id.clone(),
                BrainEvent::ToolCall(ToolCallEnvelope {
                    tool_call_id: tool_call_id.to_string(),
                    name: self.definition.name.clone(),
                    arguments: params,
                    tainted: false,
                }),
            ),
        );

        let result = tokio::select! {
            _ = cancel.cancelled() => {
                self.pending_tools.lock().await.remove(&pending_key);
                return Err(AgentToolError::Message("ask_user cancelled".to_string()));
            }
            result = result_rx => {
                result.map_err(|_| AgentToolError::Message("ask_user result channel closed".to_string()))?
            }
        };
        if !result.ok {
            return Err(AgentToolError::Message(
                result
                    .error
                    .unwrap_or_else(|| "user cancelled the question".to_string()),
            ));
        }
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(result.content.to_string())],
            details: result.content,
            terminate: None,
        })
    }
}

struct NotificationTool {
    definition: pie_ai::Tool,
    session_id: String,
    request_id: String,
    tx: mpsc::UnboundedSender<ResponseEnvelope>,
}

impl NotificationTool {
    fn new(
        session_id: String,
        request_id: String,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
    ) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: "notification".to_string(),
                description: "Emit a concise in-app user notification for a milestone, completion, or blocked state. Never include secrets or tainted browser data.".to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["body"],
                    "properties": {
                        "body": {
                            "type": "string",
                            "description": "Short notification body shown to the user."
                        },
                        "title": {
                            "type": "string",
                            "description": "Optional short title."
                        },
                        "level": {
                            "type": "string",
                            "enum": ["info", "success", "warning", "error"],
                            "description": "Notification severity."
                        },
                        "category": {
                            "type": "string",
                            "description": "Optional compact category such as task, browser, files, or auth."
                        }
                    }
                }),
            },
            session_id,
            request_id,
            tx,
        }
    }
}

#[async_trait]
impl AgentTool for NotificationTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let body = required_string(&params, "body")?.trim();
        if body.is_empty() {
            return Err(AgentToolError::Message(
                "`notification.body` must not be empty".to_string(),
            ));
        }
        let (body, body_truncated) = truncate_chars(body, MAX_NOTIFICATION_BODY_CHARS);
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NOTIFICATION_TITLE_CHARS).0);
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| matches!(*value, "info" | "success" | "warning" | "error"))
            .map(str::to_string);
        let category = params
            .get("category")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NOTIFICATION_CATEGORY_CHARS).0);
        let notification = NotificationInfo {
            body,
            title,
            level,
            category,
        };
        emit_response(
            &self.tx,
            ResponseEnvelope::session_event(
                Some(self.request_id.clone()),
                self.session_id.clone(),
                BrainEvent::Notification(notification.clone()),
            ),
        );
        let details = json!({
            "notification": notification,
            "truncated": body_truncated
        });
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(details.to_string())],
            details,
            terminate: None,
        })
    }
}

struct GetTimeTool {
    definition: pie_ai::Tool,
}

impl GetTimeTool {
    fn new() -> Self {
        Self {
            definition: pie_ai::Tool {
                name: "get_time".to_string(),
                description: "Return the current local and UTC time from the bundled Stead brain helper. Use when relative dates, scheduling, or time-sensitive browsing tasks matter.".to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                }),
            },
        }
    }
}

#[async_trait]
impl AgentTool for GetTimeTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let utc = Utc::now();
        let local = Local::now();
        let details = json!({
            "utc": utc.to_rfc3339(),
            "local": local.to_rfc3339(),
            "unix_timestamp": utc.timestamp(),
            "utc_offset_seconds": local.offset().local_minus_utc(),
            "source": "stead-brain-helper"
        });
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(details.to_string())],
            details,
            terminate: None,
        })
    }
}

struct WebFetchTool {
    definition: pie_ai::Tool,
    client: reqwest::Client,
}

impl WebFetchTool {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent(format!("SteadBrain/{BRAIN_VERSION}"))
            .build()
            .expect("WebFetch HTTP client should build");
        Self {
            definition: pie_ai::Tool {
                name: "WebFetch".to_string(),
                description: "Credentialless capped HTTP(S) fetch for public pages and docs. It sends no browser cookies and must not be used for logged-in browser state.".to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["url"],
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "HTTP or HTTPS URL to fetch without browser credentials."
                        },
                        "max_bytes": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": WEB_FETCH_HARD_MAX_BYTES,
                            "description": "Optional response byte cap. Values above the hard cap are clamped."
                        }
                    }
                }),
            },
            client,
        }
    }

    async fn fetch(
        &self,
        params: Value,
        cancel: CancellationToken,
    ) -> std::result::Result<Value, AgentToolError> {
        let url = required_string(&params, "url")?;
        let parsed = reqwest::Url::parse(url)
            .map_err(|error| AgentToolError::Message(format!("invalid url: {error}")))?;
        match parsed.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(AgentToolError::Message(format!(
                    "WebFetch only supports http/https URLs, not `{scheme}`"
                )));
            }
        }
        let max_bytes = web_fetch_max_bytes(&params)?;
        let request = self.client.get(parsed.clone());
        let mut response = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(AgentToolError::Message("WebFetch cancelled".to_string()));
            }
            response = request.send() => {
                response.map_err(|error| AgentToolError::Message(format!("WebFetch request failed: {error}")))?
            }
        };
        let status = response.status();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_length = response.content_length();

        let mut body = Vec::new();
        let mut truncated = false;
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => {
                    return Err(AgentToolError::Message("WebFetch cancelled".to_string()));
                }
                chunk = response.chunk() => {
                    chunk.map_err(|error| AgentToolError::Message(format!("WebFetch read failed: {error}")))?
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            if body.len() + chunk.len() > max_bytes {
                let remaining = max_bytes.saturating_sub(body.len());
                if remaining > 0 {
                    body.extend_from_slice(&chunk[..remaining]);
                }
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }

        let text_lossy = String::from_utf8_lossy(&body);
        let (text, text_truncated) = truncate_chars(&text_lossy, WEB_FETCH_MAX_TEXT_CHARS);
        Ok(json!({
            "url": url,
            "final_url": final_url,
            "status": status.as_u16(),
            "ok": status.is_success(),
            "content_type": content_type,
            "content_length": content_length,
            "bytes_read": body.len(),
            "byte_cap": max_bytes,
            "truncated": truncated,
            "text_truncated": text_truncated,
            "text": text
        }))
    }
}

#[async_trait]
impl AgentTool for WebFetchTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let details = self.fetch(params, cancel).await?;
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(details.to_string())],
            details,
            terminate: None,
        })
    }
}

struct SkillInvocationTool {
    definition: pie_ai::Tool,
    skills: Arc<Vec<Skill>>,
}

impl SkillInvocationTool {
    fn new(skills: Vec<Skill>) -> Self {
        Self {
            definition: pie_ai::Tool {
                name: "Skill".to_string(),
                description: "Load the full markdown body for a relevant Stead skill.".to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name"],
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "The skill name from the <skills> catalog."
                        },
                        "additional_instructions": {
                            "type": "string",
                            "description": "Optional extra context to append to the skill invocation."
                        }
                    }
                }),
            },
            skills: Arc::new(skills),
        }
    }
}

#[async_trait]
impl AgentTool for SkillInvocationTool {
    fn definition(&self) -> &pie_ai::Tool {
        &self.definition
    }

    fn label(&self) -> &str {
        &self.definition.name
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> std::result::Result<AgentToolResult, AgentToolError> {
        let name = required_string(&params, "name")?;
        let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
            return Err(AgentToolError::Message(format!("skill not found: {name}")));
        };
        if skill.disable_model_invocation {
            return Err(AgentToolError::Message(format!(
                "skill is catalog-only and cannot be invoked by the model: {name}"
            )));
        }
        let additional = params
            .get("additional_instructions")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        let invocation = format_skill_invocation(skill, additional);
        let details = json!({
            "name": skill.name,
            "source": skill.source.label(),
            "file_path": skill.file_path,
            "content_chars": skill.content.chars().count()
        });
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(invocation)],
            details,
            terminate: None,
        })
    }
}

impl BrainCore {
    pub async fn initialize(params: InitializeParams) -> Result<(Self, ReadyInfo)> {
        let config = BrainConfig::from_initialize(params);
        let agent_root = config.agent_root();
        tokio::fs::create_dir_all(agent_root.join("sessions")).await?;
        tokio::fs::create_dir_all(agent_root.join("memory")).await?;
        tokio::fs::create_dir_all(agent_root.join("skills")).await?;
        ensure_file_exists(agent_root.join("AGENTS.md")).await?;
        ensure_file_exists(agent_root.join("SOUL.md")).await?;

        let sessions = SessionStore::new(agent_root.join("sessions"));
        let files = FileAccess::new(agent_root.join("sessions"), &config.approved_roots).await?;
        let memory = MemoryStore::new(agent_root.join("memory")).await?;
        let auth = ProviderAuthStore::open(&agent_root).await?;
        let ready = ReadyInfo {
            brain_version: BRAIN_VERSION.to_string(),
            pie_commit: pie_commit().to_string(),
            app_support_dir: config.app_support_dir.clone(),
        };
        Ok((
            Self {
                config,
                sessions,
                files,
                memory,
                pending_tools: Arc::new(Mutex::new(HashMap::new())),
                active_turns: Arc::new(Mutex::new(HashMap::new())),
                auth,
            },
            ready,
        ))
    }

    pub fn config(&self) -> &BrainConfig {
        &self.config
    }

    pub fn files(&self) -> &FileAccess {
        &self.files
    }

    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    pub async fn session_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        self.sessions.messages(session_id).await
    }

    pub async fn create_session(
        &self,
        request_id: String,
        params: CreateSessionParams,
    ) -> Result<Vec<ResponseEnvelope>> {
        let session = self.sessions.create(params).await?;
        Ok(vec![ResponseEnvelope::session_event(
            Some(request_id),
            session.id.clone(),
            BrainEvent::SessionCreated { session },
        )])
    }

    pub async fn list_sessions(&self, request_id: String) -> Result<Vec<ResponseEnvelope>> {
        let sessions = self.sessions.list().await?;
        Ok(vec![ResponseEnvelope::event(
            Some(request_id),
            BrainEvent::Sessions { sessions },
        )])
    }

    pub async fn load_session(
        &self,
        request_id: String,
        session_id: String,
    ) -> Result<Vec<ResponseEnvelope>> {
        let session = self.sessions.load(&session_id).await?;
        Ok(vec![ResponseEnvelope::session_event(
            Some(request_id),
            session_id,
            BrainEvent::SessionLoaded { session },
        )])
    }

    pub async fn send_message(
        &self,
        request_id: String,
        params: SendMessageParams,
    ) -> Result<Vec<ResponseEnvelope>> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.send_message_stream(request_id, params, tx).await?;
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        Ok(events)
    }

    pub async fn send_message_stream(
        &self,
        request_id: String,
        params: SendMessageParams,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
    ) -> Result<()> {
        let session_info = self.sessions.load(&params.session_id).await?;
        if let Some((name, arguments)) = parse_tool_command(&params.text) {
            let tool_call_id = format!("tool_{}", Uuid::new_v4().simple());
            emit_response(
                &tx,
                ResponseEnvelope::session_event(
                    Some(request_id.clone()),
                    session_info.id.clone(),
                    BrainEvent::ToolStatus(ToolStatus {
                        tool_call_id: tool_call_id.clone(),
                        status: "requested".to_string(),
                        message: Some("Waiting for browser-mediated tool result.".to_string()),
                    }),
                ),
            );
            emit_response(
                &tx,
                ResponseEnvelope::session_event(
                    Some(request_id),
                    session_info.id,
                    BrainEvent::ToolCall(ToolCallEnvelope {
                        tool_call_id,
                        name,
                        arguments,
                        tainted: false,
                    }),
                ),
            );
            return Ok(());
        }

        let model = resolve_model(params.model.as_ref())?;
        self.auth.prepare_model_credential(&model).await?;
        let stored_messages = self.sessions.messages(&session_info.id).await?;
        let (pie_session, seeded_count) = seed_pie_session(&stored_messages).await?;
        let skills = self.load_skills().await;
        let mut options = AgentHarnessOptions::new(model.clone(), pie_session.clone());
        options.system_prompt = self.system_prompt().await?;
        options.skills = skills.clone();
        options.tools = self.agent_tools(&session_info.id, &request_id, tx.clone(), skills);
        options.stream_fn = Some(stead_stream_fn(self.auth.clone()));
        options.thinking_level = if model.reasoning {
            ThinkingLevel::Medium
        } else {
            ThinkingLevel::Off
        };
        options.turn_continuation_cap = Some(0);

        let harness = Arc::new(AgentHarness::new(options));
        harness
            .rehydrate_from_session()
            .await
            .map_err(|error| BrainError::AgentRun(error.to_string()))?;

        let collector = Arc::new(TurnEventCollector::default());
        let _unsubscribe = harness.subscribe(turn_event_listener(
            tx.clone(),
            request_id.clone(),
            session_info.id.clone(),
            collector.clone(),
        ));

        self.register_active_turn(&session_info.id, &request_id, harness.clone())
            .await?;
        let run = harness.prompt(params.text.clone()).await;
        self.unregister_active_turn(&session_info.id).await;
        self.persist_new_pie_messages(&session_info.id, &pie_session, seeded_count, &params)
            .await?;

        if let Err(error) = run {
            let message = error.to_string();
            if is_abort_error(&message) {
                emit_response(
                    &tx,
                    ResponseEnvelope::session_event(
                        Some(request_id.clone()),
                        session_info.id.clone(),
                        BrainEvent::ToolStatus(ToolStatus {
                            tool_call_id: "turn".to_string(),
                            status: "cancelled".to_string(),
                            message: None,
                        }),
                    ),
                );
                emit_response(
                    &tx,
                    ResponseEnvelope::session_event(
                        Some(request_id),
                        session_info.id,
                        BrainEvent::AssistantDone(AssistantDone {
                            stop_reason: "cancelled".to_string(),
                            response_id: None,
                        }),
                    ),
                );
                return Ok(());
            }
            emit_response(
                &tx,
                ResponseEnvelope::session_event(
                    Some(request_id.clone()),
                    session_info.id.clone(),
                    BrainEvent::Error(ErrorInfo {
                        code: "agent_run_failed".to_string(),
                        message: message.clone(),
                    }),
                ),
            );
            emit_response(
                &tx,
                ResponseEnvelope::session_event(
                    Some(request_id),
                    session_info.id,
                    BrainEvent::AssistantDone(AssistantDone {
                        stop_reason: "error".to_string(),
                        response_id: None,
                    }),
                ),
            );
            return Ok(());
        }

        let done = collector.done();
        emit_response(
            &tx,
            ResponseEnvelope::session_event(
                Some(request_id),
                session_info.id,
                BrainEvent::AssistantDone(done),
            ),
        );
        Ok(())
    }

    pub async fn accept_tool_result(
        &self,
        request_id: String,
        result: ToolResultEnvelope,
    ) -> Result<Vec<ResponseEnvelope>> {
        let pending_key = pending_tool_key(&result.session_id, &result.tool_call_id);
        if let Some(sender) = self.pending_tools.lock().await.remove(&pending_key) {
            let ok = result.result.ok;
            let error = result.result.error.clone();
            let _ = sender.send(result.result);
            return Ok(vec![ResponseEnvelope::session_event(
                Some(request_id),
                result.session_id,
                BrainEvent::ToolStatus(ToolStatus {
                    tool_call_id: result.tool_call_id,
                    status: if ok { "completed" } else { "failed" }.to_string(),
                    message: error,
                }),
            )]);
        }

        let content = if result.result.ok {
            "Tool result received."
        } else {
            "Tool result failed."
        };
        self.sessions
            .append_message(
                &result.session_id,
                "tool",
                content,
                json!({
                    "tool_call_id": result.tool_call_id,
                    "ok": result.result.ok,
                    "tainted": result.result.tainted
                }),
            )
            .await?;
        Ok(vec![ResponseEnvelope::session_event(
            Some(request_id),
            result.session_id,
            BrainEvent::ToolStatus(ToolStatus {
                tool_call_id: result.tool_call_id,
                status: if result.result.ok {
                    "completed"
                } else {
                    "failed"
                }
                .to_string(),
                message: result.result.error,
            }),
        )])
    }

    pub async fn cancel_turn(
        &self,
        request_id: String,
        session_id: String,
    ) -> Result<Vec<ResponseEnvelope>> {
        self.sessions.load(&session_id).await?;
        let active = self.active_turns.lock().await.get(&session_id).cloned();
        let (status, message) = if let Some(turn) = active {
            turn.harness.abort();
            (
                "cancelling",
                Some(format!("Cancelling active turn {}.", turn.request_id)),
            )
        } else {
            (
                "not_running",
                Some("No active turn for this session.".to_string()),
            )
        };
        Ok(vec![ResponseEnvelope::session_event(
            Some(request_id.clone()),
            session_id.clone(),
            BrainEvent::ToolStatus(ToolStatus {
                tool_call_id: "turn".to_string(),
                status: status.to_string(),
                message,
            }),
        )])
    }

    async fn register_active_turn(
        &self,
        session_id: &str,
        request_id: &str,
        harness: Arc<AgentHarness>,
    ) -> Result<()> {
        let mut active = self.active_turns.lock().await;
        if active.contains_key(session_id) {
            return Err(BrainError::InvalidRequest(format!(
                "session {session_id} already has an active turn"
            )));
        }
        active.insert(
            session_id.to_string(),
            ActiveTurn {
                request_id: request_id.to_string(),
                harness,
            },
        );
        Ok(())
    }

    async fn unregister_active_turn(&self, session_id: &str) {
        self.active_turns.lock().await.remove(session_id);
    }

    pub async fn list_provider_auth(&self, request_id: String) -> Result<Vec<ResponseEnvelope>> {
        Ok(vec![ResponseEnvelope::event(
            Some(request_id),
            BrainEvent::ProviderAuthStatus {
                providers: self.auth.statuses(),
            },
        )])
    }

    pub async fn list_models(&self, request_id: String) -> Result<Vec<ResponseEnvelope>> {
        Ok(vec![ResponseEnvelope::event(
            Some(request_id),
            BrainEvent::ModelCatalog {
                providers: model_catalog(&self.auth),
            },
        )])
    }

    pub async fn set_provider_credential(
        &self,
        request_id: String,
        params: stead_brain_protocol::SetProviderCredentialParams,
    ) -> Result<Vec<ResponseEnvelope>> {
        let status = self
            .auth
            .set_credential(params.provider, params.credential)
            .await?;
        Ok(vec![ResponseEnvelope::event(
            Some(request_id),
            BrainEvent::ProviderAuthCompleted { status },
        )])
    }

    pub async fn import_codex_auth(
        &self,
        request_id: String,
        params: stead_brain_protocol::ImportCodexAuthParams,
    ) -> Result<Vec<ResponseEnvelope>> {
        let status = self.auth.import_codex_auth(params.path).await?;
        Ok(vec![ResponseEnvelope::event(
            Some(request_id),
            BrainEvent::ProviderAuthCompleted { status },
        )])
    }

    pub async fn clear_provider_credential(
        &self,
        request_id: String,
        provider: String,
    ) -> Result<Vec<ResponseEnvelope>> {
        Ok(vec![ResponseEnvelope::event(
            Some(request_id),
            BrainEvent::ProviderAuthStatus {
                providers: self.auth.clear(&provider).await?,
            },
        )])
    }

    pub async fn start_provider_oauth(
        &self,
        request_id: String,
        params: stead_brain_protocol::StartProviderOAuthParams,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
    ) -> Result<()> {
        self.auth.start_oauth(request_id, params, tx).await
    }

    fn agent_tools(
        &self,
        session_id: &str,
        request_id: &str,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
        skills: Vec<Skill>,
    ) -> Vec<Arc<dyn AgentTool>> {
        let bridge = Arc::new(ProtocolBrowserToolBridge {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            pending_tools: self.pending_tools.clone(),
            tx: tx.clone(),
        });
        let mut tools = browser_tools(bridge);
        tools.extend(file_tools_for_session(
            Arc::new(self.files.clone()),
            Some(session_id.to_string()),
        ));
        tools.extend(memory_tools(Arc::new(self.memory.clone())));
        tools.extend(user_prompt_tools(
            session_id.to_string(),
            request_id.to_string(),
            self.pending_tools.clone(),
            tx,
        ));
        tools.extend(local_tools());
        if !skills.is_empty() {
            tools.push(Arc::new(SkillInvocationTool::new(skills)) as Arc<dyn AgentTool>);
        }
        tools
    }

    async fn system_prompt(&self) -> Result<String> {
        let mut prompt = STEAD_SYSTEM_PROMPT.to_string();
        for (filename, tag) in [
            ("AGENTS.md", "local_agent_instructions"),
            ("SOUL.md", "local_persona_notes"),
        ] {
            if let Some(content) = read_optional_instruction_file(
                self.config.agent_root().join(filename),
                MAX_INSTRUCTION_FILE_BYTES,
            )
            .await?
            {
                prompt.push_str("\n\n<");
                prompt.push_str(tag);
                prompt.push_str(">\n");
                prompt.push_str(content.trim());
                prompt.push_str("\n</");
                prompt.push_str(tag);
                prompt.push('>');
            }
        }
        if let Some(memory) = self.memory.prompt_block().await? {
            prompt.push_str("\n\n");
            prompt.push_str(&memory);
        }
        Ok(prompt)
    }

    async fn load_skills(&self) -> Vec<Skill> {
        load_stead_skills(self.config.agent_root().join("skills")).await
    }

    async fn persist_new_pie_messages(
        &self,
        session_id: &str,
        pie_session: &Session,
        seeded_count: usize,
        params: &SendMessageParams,
    ) -> Result<()> {
        let entries = pie_session
            .entries()
            .await
            .map_err(|error| BrainError::AgentRun(error.to_string()))?;
        let mut seen_messages = 0usize;
        for entry in entries {
            let pie_agent_core::SessionTreeEntry::Message { message, .. } = entry else {
                continue;
            };
            if seen_messages < seeded_count {
                seen_messages += 1;
                continue;
            }
            seen_messages += 1;
            if let Some((role, content, mut metadata)) = stored_message_from_agent(message) {
                if role == "user" {
                    metadata["tab_context"] =
                        serde_json::to_value(&params.tab_context).unwrap_or(Value::Null);
                }
                self.sessions
                    .append_message(session_id, &role, &content, metadata)
                    .await?;
            }
        }
        Ok(())
    }
}

fn browser_tool_description(name: &str) -> &'static str {
    match name {
        "browser.list_tabs" => "List browser tabs visible to the agent.",
        "browser.snapshot" => "Return a bounded accessibility snapshot for a tab.",
        "browser.probe_node" => "Probe DOM/style details for one referenced node.",
        "browser.screenshot" => "Capture an opt-in screenshot through the browser broker.",
        "browser.click" => "Click an accessibility node by stable reference.",
        "browser.fill" => "Fill an accessibility node by stable reference.",
        "browser.focus" => "Focus an accessibility node by stable reference.",
        "browser.scroll_into_view" => "Scroll an accessibility node into view.",
        "browser.navigate" => "Navigate a tab through the browser broker.",
        "browser.open_tab" => "Open an agent-owned browser tab.",
        "browser.close_tab" => "Close an agent-owned browser tab.",
        "browser.eval" => "Run broker-gated isolated-world JavaScript.",
        "browser.key" => "Send broker-gated trusted keyboard input.",
        "browser.mouse_click" => "Send broker-gated trusted mouse click input.",
        "browser.mouse_move" => "Send broker-gated trusted mouse move input.",
        "browser.mouse_down" => "Send broker-gated trusted mouse down input.",
        "browser.mouse_up" => "Send broker-gated trusted mouse up input.",
        "browser.mouse_drag" => "Send broker-gated trusted mouse drag input.",
        "browser.scroll" => "Send broker-gated trusted wheel input.",
        "browser.handle_dialog" => "Accept, dismiss, or respond to a browser dialog.",
        "browser.handle_file_chooser" => "Handle a file chooser through file-access gates.",
        "browser.mark_credential_injection" => {
            "Mark a frame tainted after third-party credential injection."
        }
        "browser.list_credentials" => "List opaque credential handles for an origin.",
        "browser.fill_credential" => "Fill credential fields through the Vault broker.",
        "browser.fill_totp" => "Fill a TOTP field through the Vault broker.",
        _ => "Call a browser-mediated Stead tool.",
    }
}

fn frame_ref_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["tab_id", "frame_token", "snapshot_generation"],
        "properties": {
            "tab_id": { "type": "integer" },
            "frame_token": { "type": "string" },
            "snapshot_generation": { "type": "integer", "minimum": 0 }
        }
    })
}

fn node_ref_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["frame", "ax_node_id"],
        "properties": {
            "frame": frame_ref_schema(),
            "ax_node_id": { "type": "integer" }
        }
    })
}

fn point_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["x", "y"],
        "properties": {
            "x": { "type": "integer" },
            "y": { "type": "integer" }
        }
    })
}

fn credential_ref_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["handle"],
        "properties": {
            "handle": { "type": "string" },
            "label": { "type": "string" },
            "source": { "type": "string" },
            "has_totp": { "type": "boolean" },
            "has_passkey": { "type": "boolean" }
        }
    })
}

fn browser_tool_parameters(name: &str) -> Value {
    match name {
        "browser.list_tabs" => json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
        "browser.snapshot" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id"],
            "properties": {
                "tab_id": { "type": "integer" },
                "max_nodes": { "type": "integer", "minimum": 1 },
                "include_bounds": { "type": "boolean" },
                "include_values": { "type": "boolean" }
            }
        }),
        "browser.probe_node" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["ref"],
            "properties": { "ref": node_ref_schema() }
        }),
        "browser.screenshot" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id"],
            "properties": {
                "tab_id": { "type": "integer" },
                "ref": node_ref_schema()
            }
        }),
        "browser.click" | "browser.focus" | "browser.scroll_into_view" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["ref"],
            "properties": { "ref": node_ref_schema() }
        }),
        "browser.fill" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["ref", "value"],
            "properties": {
                "ref": node_ref_schema(),
                "value": { "type": "string" }
            }
        }),
        "browser.navigate" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id", "url"],
            "properties": {
                "tab_id": { "type": "integer" },
                "url": { "type": "string" }
            }
        }),
        "browser.open_tab" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url"],
            "properties": {
                "url": { "type": "string" },
                "agent_owned": { "type": "boolean" }
            }
        }),
        "browser.close_tab" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id"],
            "properties": { "tab_id": { "type": "integer" } }
        }),
        "browser.eval" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["frame", "js"],
            "properties": {
                "frame": frame_ref_schema(),
                "js": { "type": "string" }
            }
        }),
        "browser.key" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id", "key"],
            "properties": {
                "tab_id": { "type": "integer" },
                "key": { "type": "string" },
                "modifiers": { "type": "integer" }
            }
        }),
        "browser.mouse_click"
        | "browser.mouse_move"
        | "browser.mouse_down"
        | "browser.mouse_up" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id", "point"],
            "properties": {
                "tab_id": { "type": "integer" },
                "point": point_schema(),
                "button": { "type": "integer" },
                "click_count": { "type": "integer", "minimum": 1 }
            }
        }),
        "browser.mouse_drag" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id", "from", "to"],
            "properties": {
                "tab_id": { "type": "integer" },
                "from": point_schema(),
                "to": point_schema(),
                "button": { "type": "integer" },
                "steps": { "type": "integer", "minimum": 1 }
            }
        }),
        "browser.scroll" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id", "dx", "dy"],
            "properties": {
                "tab_id": { "type": "integer" },
                "dx": { "type": "integer" },
                "dy": { "type": "integer" }
            }
        }),
        "browser.handle_dialog" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["handle", "accept"],
            "properties": {
                "handle": { "type": "string" },
                "accept": { "type": "boolean" },
                "prompt_text": { "type": "string" }
            }
        }),
        "browser.handle_file_chooser" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["handle", "paths"],
            "properties": {
                "handle": { "type": "string" },
                "paths": { "type": "array", "items": { "type": "string" } }
            }
        }),
        "browser.mark_credential_injection" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["frame"],
            "properties": { "frame": frame_ref_schema() }
        }),
        "browser.list_credentials" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tab_id", "origin"],
            "properties": {
                "tab_id": { "type": "integer" },
                "origin": { "type": "string" }
            }
        }),
        "browser.fill_credential" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["credential", "username_field", "password_field"],
            "properties": {
                "credential": credential_ref_schema(),
                "username_field": node_ref_schema(),
                "password_field": node_ref_schema()
            }
        }),
        "browser.fill_totp" => json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["credential", "field"],
            "properties": {
                "credential": credential_ref_schema(),
                "field": node_ref_schema()
            }
        }),
        _ => json!({
            "type": "object",
            "additionalProperties": true
        }),
    }
}

fn file_tool_description(name: &str) -> &'static str {
    match name {
        "files.list" => "List files inside a session root or explicitly approved folder.",
        "files.read" => "Read a capped UTF-8 file inside a session root or approved folder.",
        "files.search" => "Regex-search capped files inside a session root or approved folder.",
        "files.write" => "Write a capped file under a session root or explicitly approved folder.",
        _ => "Call a scoped Stead file tool.",
    }
}

#[derive(Clone)]
struct ProtocolBrowserToolBridge {
    session_id: String,
    request_id: String,
    pending_tools: PendingToolResults,
    tx: mpsc::UnboundedSender<ResponseEnvelope>,
}

#[async_trait]
impl BrowserToolBridge for ProtocolBrowserToolBridge {
    async fn call_browser_tool(
        &self,
        tool_call_id: &str,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<ToolResultPayload> {
        let pending_key = pending_tool_key(&self.session_id, tool_call_id);
        let (result_tx, result_rx) = oneshot::channel();
        self.pending_tools
            .lock()
            .await
            .insert(pending_key.clone(), result_tx);

        emit_response(
            &self.tx,
            ResponseEnvelope::session_event(
                Some(self.request_id.clone()),
                self.session_id.clone(),
                BrainEvent::ToolCall(ToolCallEnvelope {
                    tool_call_id: tool_call_id.to_string(),
                    name: name.to_string(),
                    arguments,
                    tainted: false,
                }),
            ),
        );

        tokio::select! {
            _ = cancel.cancelled() => {
                self.pending_tools.lock().await.remove(&pending_key);
                Err(BrainError::AgentRun(format!("browser tool cancelled: {name}")))
            }
            result = result_rx => {
                result.map_err(|_| BrainError::AgentRun(format!("browser tool result channel closed: {name}")))
            }
        }
    }
}

#[derive(Default)]
struct TurnEventCollector {
    final_stop_reason: std::sync::Mutex<Option<String>>,
    response_id: std::sync::Mutex<Option<String>>,
    emitted_text_delta: std::sync::Mutex<bool>,
}

impl TurnEventCollector {
    fn reset_text_delta(&self) {
        *self
            .emitted_text_delta
            .lock()
            .expect("delta mutex poisoned") = false;
    }

    fn record_text_delta(&self) {
        *self
            .emitted_text_delta
            .lock()
            .expect("delta mutex poisoned") = true;
    }

    fn emitted_text_delta(&self) -> bool {
        *self
            .emitted_text_delta
            .lock()
            .expect("delta mutex poisoned")
    }

    fn record_assistant(&self, message: &pie_ai::AssistantMessage) {
        *self.final_stop_reason.lock().expect("stop mutex poisoned") =
            Some(stop_reason_string(message.stop_reason).to_string());
        *self.response_id.lock().expect("response mutex poisoned") = message.response_id.clone();
    }

    fn done(&self) -> AssistantDone {
        AssistantDone {
            stop_reason: self
                .final_stop_reason
                .lock()
                .expect("stop mutex poisoned")
                .clone()
                .unwrap_or_else(|| "stop".to_string()),
            response_id: self
                .response_id
                .lock()
                .expect("response mutex poisoned")
                .clone(),
        }
    }
}

fn turn_event_listener(
    tx: mpsc::UnboundedSender<ResponseEnvelope>,
    request_id: String,
    session_id: String,
    collector: Arc<TurnEventCollector>,
) -> pie_agent_core::AgentListener {
    Arc::new(move |event, _cancel| {
        let tx = tx.clone();
        let request_id = request_id.clone();
        let session_id = session_id.clone();
        let collector = collector.clone();
        Box::pin(async move {
            match event {
                AgentEvent::MessageStart {
                    message: AgentMessage::Llm(pie_ai::Message::Assistant(_)),
                } => {
                    collector.reset_text_delta();
                }
                AgentEvent::MessageUpdate {
                    assistant_message_event,
                    ..
                } => {
                    if let pie_ai::AssistantMessageEvent::TextDelta { delta, .. } =
                        assistant_message_event
                    {
                        if !delta.is_empty() {
                            collector.record_text_delta();
                            emit_response(
                                &tx,
                                ResponseEnvelope::session_event(
                                    Some(request_id),
                                    session_id,
                                    BrainEvent::AssistantDelta { text: delta },
                                ),
                            );
                        }
                    }
                }
                AgentEvent::MessageEnd {
                    message: AgentMessage::Llm(pie_ai::Message::Assistant(assistant)),
                } => {
                    collector.record_assistant(&assistant);
                    if !collector.emitted_text_delta() {
                        let text = assistant_visible_text(&assistant.content);
                        if !text.is_empty() {
                            emit_response(
                                &tx,
                                ResponseEnvelope::session_event(
                                    Some(request_id.clone()),
                                    session_id.clone(),
                                    BrainEvent::AssistantDelta { text },
                                ),
                            );
                        }
                    }
                    emit_response(
                        &tx,
                        ResponseEnvelope::session_event(
                            Some(request_id),
                            session_id,
                            BrainEvent::UsageUpdate(UsageUpdate {
                                input_tokens: assistant.usage.input,
                                output_tokens: assistant.usage.output,
                                cache_read_tokens: assistant.usage.cache_read,
                                cache_write_tokens: assistant.usage.cache_write,
                            }),
                        ),
                    );
                }
                AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    ..
                } => {
                    emit_response(
                        &tx,
                        ResponseEnvelope::session_event(
                            Some(request_id),
                            session_id,
                            BrainEvent::ToolStatus(ToolStatus {
                                tool_call_id,
                                status: "running".to_string(),
                                message: Some(tool_name),
                            }),
                        ),
                    );
                }
                AgentEvent::ToolExecutionEnd {
                    tool_call_id,
                    tool_name,
                    is_error,
                    ..
                } => {
                    emit_response(
                        &tx,
                        ResponseEnvelope::session_event(
                            Some(request_id),
                            session_id,
                            BrainEvent::ToolStatus(ToolStatus {
                                tool_call_id,
                                status: if is_error { "failed" } else { "completed" }.to_string(),
                                message: Some(tool_name),
                            }),
                        ),
                    );
                }
                _ => {}
            }
        })
    })
}

fn stead_stream_fn(auth: ProviderAuthStore) -> pie_agent_core::StreamFn {
    Arc::new(move |model, context, options| {
        let mut owned_options = options.cloned().unwrap_or_default();
        if owned_options.base.api_key.is_none() {
            if let Some(credential) = auth.credential_for_model(model) {
                owned_options.base.api_key = Some(credential.api_key);
                if credential.auth_type == CredentialAuthType::OAuth {
                    owned_options
                        .base
                        .provider_extras
                        .insert("auth_type".to_string(), Value::String("oauth".to_string()));
                }
                if let Some(account_id) = credential.account_id {
                    owned_options
                        .base
                        .provider_extras
                        .insert("chatgpt_account_id".to_string(), Value::String(account_id));
                }
            }
        }
        pie_ai::stream_simple(model, context, Some(&owned_options))
    })
}

fn resolve_model(
    selection: Option<&stead_brain_protocol::ModelSelection>,
) -> Result<pie_ai::Model> {
    let selection = selection.ok_or(BrainError::ModelNotConfigured)?;
    if selection.provider == "faux" && selection.model == "faux" {
        return Ok(build_faux_pie_model());
    }
    pie_ai::get_model(
        &pie_ai::Provider::from(selection.provider.clone()),
        &selection.model,
    )
    .ok_or_else(|| BrainError::ModelNotFound {
        provider: selection.provider.clone(),
        model: selection.model.clone(),
    })
}

struct CatalogProviderSpec {
    id: &'static str,
    label: &'static str,
    apis: &'static [&'static str],
    supports_oauth: bool,
    supports_codex_import: bool,
}

const MODEL_CATALOG_PROVIDERS: &[CatalogProviderSpec] = &[
    CatalogProviderSpec {
        id: "anthropic",
        label: "Claude",
        apis: &["anthropic-messages"],
        supports_oauth: true,
        supports_codex_import: false,
    },
    CatalogProviderSpec {
        id: "openai-codex",
        label: "Codex",
        apis: &["openai-codex-responses"],
        supports_oauth: true,
        supports_codex_import: true,
    },
    CatalogProviderSpec {
        id: "openai",
        label: "OpenAI",
        apis: &["openai-responses", "openai-completions"],
        supports_oauth: false,
        supports_codex_import: false,
    },
    CatalogProviderSpec {
        id: "google",
        label: "Gemini",
        apis: &["google-generative-ai"],
        supports_oauth: false,
        supports_codex_import: false,
    },
];

fn model_catalog(auth: &ProviderAuthStore) -> Vec<ModelCatalogProvider> {
    let auth_statuses: HashMap<String, stead_brain_protocol::ProviderAuthStatus> = auth
        .statuses()
        .into_iter()
        .map(|status| (status.provider.clone(), status))
        .collect();
    let specs_by_provider: HashMap<&'static str, &CatalogProviderSpec> = MODEL_CATALOG_PROVIDERS
        .iter()
        .map(|spec| (spec.id, spec))
        .collect();
    let mut models_by_provider: BTreeMap<String, Vec<ModelCatalogEntry>> = BTreeMap::new();

    for model in pie_ai::list_models() {
        let provider = model.provider.0.as_str();
        let Some(spec) = specs_by_provider.get(provider) else {
            continue;
        };
        if !spec.apis.iter().any(|api| *api == model.api.0.as_str()) {
            continue;
        }
        models_by_provider
            .entry(model.provider.0.clone())
            .or_default()
            .push(ModelCatalogEntry {
                id: model.id,
                name: model.name,
                api: model.api.0,
                reasoning: model.reasoning,
                input: model
                    .input
                    .into_iter()
                    .map(|input| match input {
                        pie_ai::InputModality::Text => "text".to_string(),
                        pie_ai::InputModality::Image => "image".to_string(),
                    })
                    .collect(),
                context_window: model.context_window,
                max_tokens: model.max_tokens,
            });
    }

    MODEL_CATALOG_PROVIDERS
        .iter()
        .filter_map(|spec| {
            let mut models = models_by_provider.remove(spec.id)?;
            models.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
            let auth_status = auth_statuses.get(spec.id);
            Some(ModelCatalogProvider {
                provider: spec.id.to_string(),
                label: spec.label.to_string(),
                configured: auth_status.map(|status| status.configured).unwrap_or(false),
                credential_kind: auth_status.and_then(|status| status.credential_kind.clone()),
                source: auth_status.and_then(|status| status.source.clone()),
                supports_oauth: spec.supports_oauth,
                supports_codex_import: spec.supports_codex_import,
                models,
            })
        })
        .collect()
}

async fn seed_pie_session(messages: &[StoredMessage]) -> Result<(Session, usize)> {
    let storage = Arc::new(MemorySessionStorage::new()) as Arc<dyn SessionStorage>;
    let session = Session::new(storage);
    let mut seeded = 0usize;
    for message in messages {
        if let Some(agent_message) = agent_message_from_stored(message) {
            session
                .append_message(agent_message)
                .await
                .map_err(|error| BrainError::AgentRun(error.to_string()))?;
            seeded += 1;
        }
    }
    Ok((session, seeded))
}

fn agent_message_from_stored(message: &StoredMessage) -> Option<AgentMessage> {
    match message.role.as_str() {
        "user" => Some(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text(message.content.clone()),
                timestamp: message.created_at.timestamp_millis(),
            },
        ))),
        "assistant" => {
            let provider = message
                .metadata
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let model = message
                .metadata
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Some(AgentMessage::Llm(pie_ai::Message::Assistant(
                pie_ai::AssistantMessage {
                    role: pie_ai::AssistantRole::Assistant,
                    content: vec![pie_ai::ContentBlock::text(message.content.clone())],
                    api: pie_ai::Api::from(
                        message
                            .metadata
                            .get("api")
                            .and_then(Value::as_str)
                            .unwrap_or(provider),
                    ),
                    provider: pie_ai::Provider::from(provider),
                    model: model.to_string(),
                    response_model: None,
                    response_id: message
                        .metadata
                        .get("response_id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    diagnostics: None,
                    usage: usage_from_metadata(&message.metadata),
                    stop_reason: stop_reason_from_metadata(&message.metadata),
                    error_message: message
                        .metadata
                        .get("error")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    timestamp: message.created_at.timestamp_millis(),
                },
            )))
        }
        "tool" => {
            let tool_call_id = message
                .metadata
                .get("tool_call_id")
                .and_then(Value::as_str)?
                .to_string();
            let tool_name = message
                .metadata
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            Some(AgentMessage::Llm(pie_ai::Message::ToolResult(
                pie_ai::ToolResultMessage {
                    role: pie_ai::ToolResultRole::ToolResult,
                    tool_call_id,
                    tool_name,
                    content: vec![pie_ai::UserContentBlock::text(message.content.clone())],
                    details: message.metadata.get("details").cloned(),
                    is_error: message
                        .metadata
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    timestamp: message.created_at.timestamp_millis(),
                },
            )))
        }
        _ => None,
    }
}

fn stored_message_from_agent(message: AgentMessage) -> Option<(String, String, Value)> {
    match message {
        AgentMessage::Llm(pie_ai::Message::User(user)) => Some((
            "user".to_string(),
            user_content_to_text(&user.content),
            json!({}),
        )),
        AgentMessage::Llm(pie_ai::Message::Assistant(assistant)) => Some((
            "assistant".to_string(),
            assistant_content_to_text(&assistant.content),
            json!({
                "api": assistant.api.0,
                "provider": assistant.provider.0,
                "model": assistant.model,
                "response_model": assistant.response_model,
                "response_id": assistant.response_id,
                "stop_reason": stop_reason_string(assistant.stop_reason),
                "error": assistant.error_message,
                "usage": {
                    "input": assistant.usage.input,
                    "output": assistant.usage.output,
                    "cache_read": assistant.usage.cache_read,
                    "cache_write": assistant.usage.cache_write,
                    "total_tokens": assistant.usage.total_tokens
                }
            }),
        )),
        AgentMessage::Llm(pie_ai::Message::ToolResult(tool)) => Some((
            "tool".to_string(),
            user_blocks_to_text(&tool.content),
            json!({
                "tool_call_id": tool.tool_call_id,
                "tool_name": tool.tool_name,
                "is_error": tool.is_error,
                "details": tool.details
            }),
        )),
        AgentMessage::Custom(_) => None,
    }
}

fn user_content_to_text(content: &pie_ai::UserContent) -> String {
    match content {
        pie_ai::UserContent::Text(text) => text.clone(),
        pie_ai::UserContent::Blocks(blocks) => user_blocks_to_text(blocks),
    }
}

fn user_blocks_to_text(blocks: &[pie_ai::UserContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            pie_ai::UserContentBlock::Text(text) => Some(text.text.clone()),
            pie_ai::UserContentBlock::Image(image) => Some(format!(
                "[image:{};{} base64 chars]",
                image.mime_type,
                image.data.len()
            )),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_content_to_text(blocks: &[pie_ai::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            pie_ai::ContentBlock::Text(text) => Some(text.text.clone()),
            pie_ai::ContentBlock::Thinking(_) => None,
            pie_ai::ContentBlock::Image(image) => Some(format!(
                "[image:{};{} base64 chars]",
                image.mime_type,
                image.data.len()
            )),
            pie_ai::ContentBlock::ToolCall(tool) => Some(format!(
                "[tool_call:{} {}]",
                tool.name,
                Value::Object(tool.arguments.clone())
            )),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assistant_visible_text(blocks: &[pie_ai::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            pie_ai::ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_abort_error(message: &str) -> bool {
    message == "aborted" || message.contains("browser tool cancelled:")
}

fn usage_from_metadata(metadata: &Value) -> pie_ai::Usage {
    let usage = metadata.get("usage").unwrap_or(&Value::Null);
    pie_ai::Usage {
        input: usage.get("input").and_then(Value::as_u64).unwrap_or(0),
        output: usage.get("output").and_then(Value::as_u64).unwrap_or(0),
        cache_read: usage.get("cache_read").and_then(Value::as_u64).unwrap_or(0),
        cache_write: usage
            .get("cache_write")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cost: pie_ai::UsageCost::default(),
    }
}

fn stop_reason_from_metadata(metadata: &Value) -> pie_ai::StopReason {
    match metadata.get("stop_reason").and_then(Value::as_str) {
        Some("length") => pie_ai::StopReason::Length,
        Some("tool_use") => pie_ai::StopReason::ToolUse,
        Some("error") => pie_ai::StopReason::Error,
        Some("aborted") => pie_ai::StopReason::Aborted,
        _ => pie_ai::StopReason::Stop,
    }
}

fn stop_reason_string(reason: pie_ai::StopReason) -> &'static str {
    match reason {
        pie_ai::StopReason::Stop => "stop",
        pie_ai::StopReason::Length => "length",
        pie_ai::StopReason::ToolUse => "tool_use",
        pie_ai::StopReason::Error => "error",
        pie_ai::StopReason::Aborted => "aborted",
    }
}

fn pending_tool_key(session_id: &str, tool_call_id: &str) -> String {
    format!("{session_id}:{tool_call_id}")
}

fn emit_response(tx: &mpsc::UnboundedSender<ResponseEnvelope>, response: ResponseEnvelope) {
    let _ = tx.send(response);
}

fn required_string<'a>(
    params: &'a Value,
    key: &str,
) -> std::result::Result<&'a str, AgentToolError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AgentToolError::Message(format!("missing string argument `{key}`")))
}

fn web_fetch_max_bytes(params: &Value) -> std::result::Result<usize, AgentToolError> {
    let Some(value) = params.get("max_bytes") else {
        return Ok(WEB_FETCH_DEFAULT_MAX_BYTES);
    };
    let Some(requested) = value.as_u64() else {
        return Err(AgentToolError::Message(
            "`max_bytes` must be a positive integer".to_string(),
        ));
    };
    if requested == 0 {
        return Err(AgentToolError::Message(
            "`max_bytes` must be greater than zero".to_string(),
        ));
    }
    Ok((requested as usize).min(WEB_FETCH_HARD_MAX_BYTES))
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut iter = value.chars();
    let truncated: String = iter.by_ref().take(max_chars).collect();
    let was_truncated = iter.next().is_some();
    (truncated, was_truncated)
}

fn content_bytes(params: &Value) -> std::result::Result<Vec<u8>, AgentToolError> {
    let has_text = params.get("content").is_some();
    let has_base64 = params.get("content_base64").is_some();
    match (has_text, has_base64) {
        (true, false) => Ok(required_string(params, "content")?.as_bytes().to_vec()),
        (false, true) => {
            let encoded = required_string(params, "content_base64")?;
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|e| AgentToolError::Message(format!("invalid content_base64: {e}")))
        }
        (true, true) => Err(AgentToolError::Message(
            "provide only one of `content` or `content_base64`".to_string(),
        )),
        (false, false) => Err(AgentToolError::Message(
            "missing `content` or `content_base64`".to_string(),
        )),
    }
}

fn tool_error(error: BrainError) -> AgentToolError {
    AgentToolError::Message(error.to_string())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionMeta {
    id: String,
    title: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    origin_surface: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Clone, Debug)]
struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    async fn create(&self, params: CreateSessionParams) -> Result<SessionInfo> {
        tokio::fs::create_dir_all(&self.root).await?;
        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now();
        let title = params.title.unwrap_or_else(|| "New chat".to_string());
        let session_dir = self.root.join(&id);
        tokio::fs::create_dir_all(&session_dir).await?;
        tokio::fs::create_dir_all(session_dir.join("attachments")).await?;
        tokio::fs::create_dir_all(session_dir.join("tmp")).await?;
        tokio::fs::create_dir_all(session_dir.join("artifacts")).await?;
        let meta = SessionMeta {
            id: id.clone(),
            title,
            created_at,
            updated_at: created_at,
            origin_surface: params.origin_surface,
        };
        write_json(session_dir.join("meta.json"), &meta).await?;
        tokio::fs::write(session_dir.join("messages.jsonl"), b"").await?;
        Ok(meta_to_info(meta, session_dir))
    }

    async fn list(&self) -> Result<Vec<SessionInfo>> {
        let mut sessions = Vec::new();
        let mut rd = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(meta) = read_json::<SessionMeta>(path.join("meta.json")).await {
                    sessions.push(meta_to_info(meta, path));
                }
            }
        }
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    async fn load(&self, session_id: &str) -> Result<SessionInfo> {
        if !is_safe_session_id(session_id) {
            return Err(BrainError::InvalidRequest("invalid session id".to_string()));
        }
        let path = self.root.join(session_id);
        let meta = read_json::<SessionMeta>(path.join("meta.json"))
            .await
            .map_err(|_| BrainError::SessionNotFound(session_id.to_string()))?;
        Ok(meta_to_info(meta, path))
    }

    async fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        metadata: Value,
    ) -> Result<()> {
        let info = self.load(session_id).await?;
        let message = StoredMessage {
            role: role.to_string(),
            content: content.to_string(),
            created_at: Utc::now(),
            metadata,
        };
        let mut encoded = serde_json::to_vec(&message)?;
        encoded.push(b'\n');
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(info.path.join("messages.jsonl"))
            .await?;
        file.write_all(&encoded).await?;

        let mut meta = read_json::<SessionMeta>(info.path.join("meta.json")).await?;
        meta.updated_at = Utc::now();
        write_json(info.path.join("meta.json"), &meta).await
    }

    pub async fn messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let info = self.load(session_id).await?;
        let data = tokio::fs::read_to_string(info.path.join("messages.jsonl")).await?;
        let mut messages = Vec::new();
        for line in data.lines().filter(|line| !line.trim().is_empty()) {
            messages.push(serde_json::from_str(line)?);
        }
        Ok(messages)
    }
}

#[derive(Clone, Debug)]
pub struct MemoryStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub content: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemorySummary {
    pub key: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemorySearchMatch {
    pub key: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub snippet: String,
}

impl MemoryStore {
    async fn new(root: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self {
            root: canonicalize_existing(&root).await?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    async fn save(
        &self,
        name: &str,
        description: &str,
        kind: &str,
        content: &str,
    ) -> Result<MemorySummary> {
        let name = clean_memory_field(name, MAX_MEMORY_NAME_CHARS, "memory name")?;
        let description = clean_memory_field(description, 512, "memory description")?;
        let kind = clean_memory_field(kind, 64, "memory type")?;
        let content = content.trim();
        if content.is_empty() {
            return Err(BrainError::InvalidRequest(
                "memory content must not be empty".to_string(),
            ));
        }
        if content.len() > MAX_MEMORY_ENTRY_BYTES {
            return Err(BrainError::InvalidRequest(format!(
                "memory content is larger than {} bytes",
                MAX_MEMORY_ENTRY_BYTES
            )));
        }
        let key = memory_key_for_name(&name)?;
        let entry = MemoryEntry {
            key: key.clone(),
            name,
            description,
            kind,
            content: content.to_string(),
            updated_at: Utc::now(),
        };
        write_json(self.entry_path(&key), &entry).await?;
        Ok(entry.summary())
    }

    async fn list(&self) -> Result<Vec<MemorySummary>> {
        let mut entries = Vec::new();
        let mut rd = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = rd.next_entry().await? {
            if entries.len() >= MAX_MEMORY_ENTRIES {
                break;
            }
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("json") {
                continue;
            }
            let Ok(memory) = read_memory_entry(path).await else {
                continue;
            };
            entries.push(memory.summary());
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
        Ok(entries)
    }

    async fn read(&self, name: &str) -> Result<MemoryEntry> {
        let key = memory_key_for_name(name)?;
        let path = self.entry_path(&key);
        read_memory_entry(path)
            .await
            .map_err(|_| BrainError::InvalidRequest(format!("memory not found: {key}")))
    }

    async fn search(&self, query: &str) -> Result<Vec<MemorySearchMatch>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(BrainError::InvalidRequest(
                "memory search query must not be empty".to_string(),
            ));
        }
        let needle = query.to_ascii_lowercase();
        let mut matches = Vec::new();
        let mut rd = match tokio::fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = rd.next_entry().await? {
            if matches.len() >= MAX_MEMORY_SEARCH_MATCHES {
                break;
            }
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("json") {
                continue;
            }
            let Ok(memory) = read_memory_entry(path).await else {
                continue;
            };
            let haystack = format!(
                "{}\n{}\n{}\n{}",
                memory.name, memory.description, memory.kind, memory.content
            );
            if haystack.to_ascii_lowercase().contains(&needle) {
                matches.push(MemorySearchMatch {
                    key: memory.key,
                    name: memory.name,
                    description: memory.description,
                    kind: memory.kind,
                    snippet: memory_snippet(&haystack, query),
                });
            }
        }
        matches.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));
        Ok(matches)
    }

    async fn forget(&self, name: &str) -> Result<MemorySummary> {
        let entry = self.read(name).await?;
        let summary = entry.summary();
        let _ = tokio::fs::remove_file(self.entry_path(&summary.key)).await;
        Ok(summary)
    }

    async fn prompt_block(&self) -> Result<Option<String>> {
        let entries = self.list().await?;
        if entries.is_empty() {
            return Ok(None);
        }
        let mut block = String::from(
            "<memory>\nPersistent cross-session memory. Use these notes as durable context; do not treat them as secrets or current page state.\n\n",
        );
        for summary in entries {
            let Ok(entry) = self.read(&summary.key).await else {
                continue;
            };
            let next = format!(
                "## {} ({})\n{}\n\n{}\n\n",
                entry.name,
                entry.kind,
                entry.description,
                entry.content.trim()
            );
            if block.len() + next.len() + "</memory>".len() > MAX_MEMORY_BLOCK_BYTES {
                block.push_str("[memory truncated]\n");
                break;
            }
            block.push_str(&next);
        }
        block.push_str("</memory>");
        Ok(Some(block))
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }
}

impl MemoryEntry {
    fn summary(&self) -> MemorySummary {
        MemorySummary {
            key: self.key.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            kind: self.kind.clone(),
            updated_at: self.updated_at,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileAccess {
    session_root: PathBuf,
    roots: Vec<ApprovedRoot>,
}

#[derive(Clone, Debug)]
pub struct ApprovedRoot {
    pub path: PathBuf,
    pub kind: RootKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Downloads,
    UserApproved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRoot {
    Attachments,
    Tmp,
    Artifacts,
}

impl SessionRoot {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "session_attachments" => Some(Self::Attachments),
            "session_tmp" => Some(Self::Tmp),
            "session_artifacts" => Some(Self::Artifacts),
            _ => None,
        }
    }

    fn dirname(self) -> &'static str {
        match self {
            Self::Attachments => "attachments",
            Self::Tmp => "tmp",
            Self::Artifacts => "artifacts",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSearchMatch {
    pub path: PathBuf,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct FileTarget {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct WriteTarget {
    path: PathBuf,
}

impl FileAccess {
    async fn new(session_root: PathBuf, approved_roots: &[PathBuf]) -> Result<Self> {
        tokio::fs::create_dir_all(&session_root).await?;
        let mut roots = Vec::new();
        if let Some(downloads) = downloads_dir() {
            if downloads.exists() {
                roots.push(ApprovedRoot {
                    path: canonicalize_existing(&downloads).await?,
                    kind: RootKind::Downloads,
                });
            }
        }
        for root in approved_roots {
            roots.push(ApprovedRoot {
                path: canonicalize_existing(root).await?,
                kind: RootKind::UserApproved,
            });
        }
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        Ok(Self {
            session_root: canonicalize_existing(&session_root).await?,
            roots,
        })
    }

    pub fn roots(&self) -> &[ApprovedRoot] {
        &self.roots
    }

    pub async fn read_to_string(&self, target: FileTarget) -> Result<String> {
        let path = target.path;
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.len() > MAX_READ_BYTES {
            return Err(BrainError::FileAccessDenied(format!(
                "file is larger than {} bytes",
                MAX_READ_BYTES
            )));
        }
        Ok(tokio::fs::read_to_string(path).await?)
    }

    pub async fn list(&self, target: FileTarget) -> Result<Vec<PathBuf>> {
        let path = target.path;
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(path).await?;
        while let Some(entry) = rd.next_entry().await? {
            out.push(entry.path());
        }
        out.sort();
        Ok(out)
    }

    pub async fn search(&self, target: FileTarget, pattern: &str) -> Result<Vec<FileSearchMatch>> {
        let root = target.path;
        let regex = Regex::new(pattern)
            .map_err(|e| BrainError::InvalidRequest(format!("invalid regex: {e}")))?;
        let mut matches = Vec::new();
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if matches.len() >= MAX_SEARCH_MATCHES {
                break;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let candidate = entry.path();
            let Ok(candidate) = canonicalize_existing(candidate).await else {
                continue;
            };
            if !candidate.starts_with(&root) {
                continue;
            }
            let Ok(metadata) = tokio::fs::metadata(&candidate).await else {
                continue;
            };
            if metadata.len() > MAX_SEARCH_BYTES {
                continue;
            }
            let Ok(contents) = tokio::fs::read_to_string(&candidate).await else {
                continue;
            };
            for (idx, line) in contents.lines().enumerate() {
                if regex.is_match(line) {
                    matches.push(FileSearchMatch {
                        path: candidate.clone(),
                        line: idx + 1,
                        text: line.to_string(),
                    });
                    if matches.len() >= MAX_SEARCH_MATCHES {
                        break;
                    }
                }
            }
        }
        Ok(matches)
    }

    pub async fn write(&self, target: WriteTarget, contents: &[u8]) -> Result<PathBuf> {
        if contents.len() > MAX_WRITE_BYTES {
            return Err(BrainError::FileAccessDenied(format!(
                "write is larger than {} bytes",
                MAX_WRITE_BYTES
            )));
        }
        let out = target.path;
        self.ensure_existing_output_does_not_escape(&out).await?;
        tokio::fs::write(&out, contents).await?;
        Ok(out)
    }

    async fn target_from_params(
        &self,
        params: &Value,
        path_key: &str,
        allow_empty_session_path: bool,
    ) -> std::result::Result<FileTarget, AgentToolError> {
        if let Some(root) = params
            .get("root")
            .and_then(Value::as_str)
            .and_then(SessionRoot::parse)
        {
            let session_id = required_string(params, "session_id")?;
            let rel = params
                .get("path")
                .and_then(Value::as_str)
                .or_else(|| {
                    if path_key != "root" {
                        params.get(path_key).and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .unwrap_or("");
            if !allow_empty_session_path && rel.is_empty() {
                return Err(AgentToolError::Message(
                    "missing session-relative path".to_string(),
                ));
            }
            let path = self
                .resolve_session_existing(session_id, root, rel)
                .await
                .map_err(tool_error)?;
            return Ok(FileTarget { path });
        }

        let path = required_string(params, path_key)?;
        let path = self
            .resolve_existing(Path::new(path))
            .await
            .map_err(tool_error)?;
        Ok(FileTarget { path })
    }

    async fn write_target_from_params(
        &self,
        params: &Value,
    ) -> std::result::Result<WriteTarget, AgentToolError> {
        if let Some(root) = params
            .get("root")
            .and_then(Value::as_str)
            .and_then(SessionRoot::parse)
        {
            if root == SessionRoot::Attachments {
                return Err(AgentToolError::Message(
                    "session_attachments is read-only for the agent".to_string(),
                ));
            }
            let session_id = required_string(params, "session_id")?;
            let rel = required_string(params, "path")?;
            let path = self
                .resolve_session_write(session_id, root, rel)
                .await
                .map_err(tool_error)?;
            return Ok(WriteTarget { path });
        }

        let path = required_string(params, "path")?;
        let path = self
            .resolve_approved_write(Path::new(path))
            .await
            .map_err(tool_error)?;
        Ok(WriteTarget { path })
    }

    async fn resolve_session_existing(
        &self,
        session_id: &str,
        root: SessionRoot,
        rel: &str,
    ) -> Result<PathBuf> {
        let base = self.session_base(session_id, root).await?;
        let rel = safe_relative_path(rel, true)?;
        let target = base.join(rel);
        let canonical = canonicalize_existing(&target).await?;
        if canonical.starts_with(&base) {
            Ok(canonical)
        } else {
            Err(BrainError::FileAccessDenied(format!(
                "{} escapes session root",
                target.display()
            )))
        }
    }

    async fn resolve_session_write(
        &self,
        session_id: &str,
        root: SessionRoot,
        rel: &str,
    ) -> Result<PathBuf> {
        let base = self.session_base(session_id, root).await?;
        let rel = safe_relative_path(rel, false)?;
        let out = base.join(rel);
        let parent = out
            .parent()
            .ok_or_else(|| BrainError::FileAccessDenied("path has no parent".to_string()))?;
        tokio::fs::create_dir_all(parent).await?;
        let canonical_parent = canonicalize_existing(parent).await?;
        if !canonical_parent.starts_with(&base) {
            return Err(BrainError::FileAccessDenied(format!(
                "{} escapes session root",
                out.display()
            )));
        }
        self.ensure_existing_output_does_not_escape(&out).await?;
        Ok(out)
    }

    async fn resolve_approved_write(&self, path: &Path) -> Result<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| BrainError::FileAccessDenied("path has no parent".to_string()))?;
        let parent = self.resolve_existing(parent).await?;
        let filename = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| BrainError::FileAccessDenied("path has no filename".to_string()))?;
        if !is_safe_filename(filename) {
            return Err(BrainError::FileAccessDenied("unsafe filename".to_string()));
        }
        let out = parent.join(filename);
        self.ensure_existing_output_does_not_escape(&out).await?;
        Ok(out)
    }

    async fn session_base(&self, session_id: &str, root: SessionRoot) -> Result<PathBuf> {
        if !is_safe_session_id(session_id) {
            return Err(BrainError::InvalidRequest("invalid session id".to_string()));
        }
        let base = self.session_root.join(session_id).join(root.dirname());
        tokio::fs::create_dir_all(&base).await?;
        let canonical = canonicalize_existing(&base).await?;
        if canonical.starts_with(&self.session_root) {
            Ok(canonical)
        } else {
            Err(BrainError::FileAccessDenied(format!(
                "{} escapes sessions root",
                base.display()
            )))
        }
    }

    async fn ensure_existing_output_does_not_escape(&self, out: &Path) -> Result<()> {
        if tokio::fs::symlink_metadata(&out).await.is_ok() {
            let canonical_out = canonicalize_existing(&out).await?;
            if !self.is_allowed(&canonical_out) && !canonical_out.starts_with(&self.session_root) {
                return Err(BrainError::FileAccessDenied(format!(
                    "{} escapes approved roots",
                    out.display()
                )));
            }
        }
        Ok(())
    }

    async fn resolve_existing(&self, path: &Path) -> Result<PathBuf> {
        let canonical = canonicalize_existing(path).await?;
        if self.is_allowed(&canonical) {
            Ok(canonical)
        } else {
            Err(BrainError::FileAccessDenied(format!(
                "{} is outside approved roots",
                path.display()
            )))
        }
    }

    fn is_allowed(&self, canonical: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| canonical.starts_with(&root.path))
    }
}

pub fn pie_commit() -> &'static str {
    PIE_PIN
        .lines()
        .find_map(|line| {
            line.strip_prefix("commit=")
                .or_else(|| line.strip_prefix("commit: "))
        })
        .unwrap_or("unknown")
}

async fn load_stead_skills(skills_root: PathBuf) -> Vec<Skill> {
    let mut skills = builtin_stead_skills();
    let dir = skills_root.to_string_lossy().to_string();
    let env = NativeEnv::new("/");
    let mut loaded = load_skills(&env, &[dir.as_str()], CancellationToken::new()).await;
    for skill in loaded.skills.iter_mut() {
        skill.source = SkillSource::User;
    }
    skills.append(&mut loaded.skills);
    normalize_skills(&mut skills);
    skills
}

fn builtin_stead_skills() -> Vec<Skill> {
    BUILTIN_STEAD_SKILLS
        .iter()
        .filter_map(|(relative_path, raw)| builtin_skill_from_markdown(relative_path, raw))
        .collect()
}

fn builtin_skill_from_markdown(relative_path: &str, raw: &str) -> Option<Skill> {
    let (frontmatter, body) = split_frontmatter(raw);
    let mut name = None;
    let mut description = None;
    let mut disable_model_invocation = false;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "disable_model_invocation" | "disable-model-invocation" => {
                disable_model_invocation = value == "true";
            }
            _ => {}
        }
    }
    let name = name?;
    let description = description?;
    if name.trim().is_empty() || description.trim().is_empty() {
        return None;
    }
    Some(Skill {
        name,
        description,
        file_path: format!("<builtin>/stead/{relative_path}"),
        content: body.trim().to_string(),
        disable_model_invocation,
        source: SkillSource::Builtin,
    })
}

fn split_frontmatter(raw: &str) -> (&str, &str) {
    let Some(rest) = raw.strip_prefix("---") else {
        return ("", raw);
    };
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let Some(end) = rest.find("\n---") else {
        return ("", raw);
    };
    let frontmatter = &rest[..end];
    let after = &rest[end + "\n---".len()..];
    let body = after.strip_prefix('\n').unwrap_or(after);
    (frontmatter, body)
}

fn normalize_skills(skills: &mut Vec<Skill>) {
    for skill in skills.iter_mut() {
        if skill.content.len() > MAX_SKILL_CONTENT_CHARS {
            let mut boundary = MAX_SKILL_CONTENT_CHARS;
            while boundary > 0 && !skill.content.is_char_boundary(boundary) {
                boundary -= 1;
            }
            skill.content.truncate(boundary);
            skill
                .content
                .push_str("\n\n[Stead truncated this skill at the configured prompt cap.]");
        }
    }
    let mut by_name = BTreeMap::new();
    for skill in skills.drain(..) {
        by_name.insert(skill.name.clone(), skill);
    }
    skills.extend(by_name.into_values());
    if skills.len() > MAX_SKILLS {
        skills.truncate(MAX_SKILLS);
    }
}

async fn ensure_file_exists(path: PathBuf) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    if tokio::fs::try_exists(&path).await? {
        return Ok(());
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(b"").await?;
    Ok(())
}

async fn read_optional_instruction_file(path: PathBuf, max_bytes: u64) -> Result<Option<String>> {
    use tokio::io::AsyncReadExt;
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut buffer = Vec::new();
    file.take(max_bytes).read_to_end(&mut buffer).await?;
    let mut content = String::from_utf8_lossy(&buffer).to_string();
    if content.trim().is_empty() {
        return Ok(None);
    }
    if buffer.len() as u64 == max_bytes {
        content
            .push_str("\n\n[Stead truncated this instruction file at the configured prompt cap.]");
    }
    Ok(Some(content))
}

pub fn build_faux_pie_model() -> pie_ai::Model {
    pie_ai::list_models()
        .into_iter()
        .find(|model| model.provider.0 == "faux")
        .unwrap_or_else(|| pie_ai::Model {
            id: "faux".to_string(),
            name: "Faux".to_string(),
            api: pie_ai::Api("faux".to_string()),
            provider: pie_ai::Provider("faux".to_string()),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![pie_ai::InputModality::Text],
            cost: pie_ai::ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8192,
            headers: None,
            compat: None,
        })
}

pub fn make_error(
    request_id: Option<String>,
    code: &str,
    message: impl Into<String>,
) -> ResponseEnvelope {
    ResponseEnvelope::event(
        request_id,
        BrainEvent::Error(ErrorInfo {
            code: code.to_string(),
            message: message.into(),
        }),
    )
}

fn meta_to_info(meta: SessionMeta, path: PathBuf) -> SessionInfo {
    SessionInfo {
        id: meta.id,
        title: meta.title,
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        path,
    }
}

fn parse_tool_command(text: &str) -> Option<(String, Value)> {
    let rest = text.strip_prefix("/tool ")?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let args = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(serde_json::from_str)
        .transpose()
        .ok()?
        .unwrap_or_else(|| json!({}));
    Some((name.to_string(), args))
}

fn default_app_support_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library")
        .join("Application Support")
        .join("Stead")
}

fn downloads_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Downloads"))
}

async fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    tokio::fs::canonicalize(path).await.map_err(Into::into)
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T> {
    let data = tokio::fs::read(path).await?;
    Ok(serde_json::from_slice(&data)?)
}

async fn write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<()> {
    let data = serde_json::to_vec_pretty(value)?;
    tokio::fs::write(path, data).await?;
    Ok(())
}

async fn read_memory_entry(path: PathBuf) -> Result<MemoryEntry> {
    let metadata = tokio::fs::metadata(&path).await?;
    if metadata.len() > MAX_MEMORY_ENTRY_BYTES as u64 + 4096 {
        return Err(BrainError::InvalidRequest(format!(
            "{} is too large to be a memory entry",
            path.display()
        )));
    }
    read_json::<MemoryEntry>(path).await
}

fn clean_memory_field(value: &str, max_chars: usize, label: &str) -> Result<String> {
    let cleaned = value.trim();
    if cleaned.is_empty() {
        return Err(BrainError::InvalidRequest(format!(
            "{label} must not be empty"
        )));
    }
    let char_count = cleaned.chars().count();
    if char_count > max_chars {
        return Err(BrainError::InvalidRequest(format!(
            "{label} is longer than {max_chars} characters"
        )));
    }
    Ok(cleaned.to_string())
}

fn memory_key_for_name(name: &str) -> Result<String> {
    let mut out = String::with_capacity(name.len().min(MAX_MEMORY_NAME_CHARS));
    let mut prev_dash = false;
    for c in name.chars() {
        let normalized = if c.is_ascii_alphanumeric() {
            Some(c.to_ascii_lowercase())
        } else if c.is_whitespace() || c == '-' || c == '_' || c == '.' || c == '/' || c == '\\' {
            Some('-')
        } else {
            None
        };
        let Some(c) = normalized else {
            continue;
        };
        if c == '-' {
            if !prev_dash && !out.is_empty() {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
        if out.len() >= 80 {
            break;
        }
    }
    let key = out.trim_matches('-').to_string();
    if key.is_empty() {
        return Err(BrainError::InvalidRequest(
            "memory name did not produce a safe key".to_string(),
        ));
    }
    Ok(key)
}

fn memory_snippet(haystack: &str, query: &str) -> String {
    let lower = haystack.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    let byte_idx = lower.find(&needle).unwrap_or(0);
    let start = haystack[..byte_idx]
        .char_indices()
        .rev()
        .nth(80)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let end = haystack[byte_idx..]
        .char_indices()
        .nth(240)
        .map(|(idx, _)| byte_idx + idx)
        .unwrap_or(haystack.len());
    haystack[start..end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_safe_session_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_safe_filename(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('/')
        && !value.contains('\\')
        && value != "."
        && value != ".."
}

fn safe_relative_path(value: &str, allow_empty: bool) -> Result<PathBuf> {
    if value.is_empty() {
        return if allow_empty {
            Ok(PathBuf::new())
        } else {
            Err(BrainError::FileAccessDenied("path is empty".to_string()))
        };
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(BrainError::FileAccessDenied(
            "session-relative path must not be absolute".to_string(),
        ));
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(BrainError::FileAccessDenied(format!(
                    "unsafe session-relative path: {value}"
                )));
            }
        }
    }
    if out.as_os_str().is_empty() && !allow_empty {
        return Err(BrainError::FileAccessDenied("path is empty".to_string()));
    }
    Ok(out)
}

#[allow(dead_code)]
fn _protocol_version_marker() -> u32 {
    PROTOCOL_VERSION
}

#[allow(dead_code)]
fn _pie_type_marker(_: pie_agent_core::harness::agent_harness::AgentHarnessOptions) {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use pie_agent_core::harness::agent_harness::AgentHarnessOptions;
    use pie_agent_core::harness::session::memory_storage::MemorySessionStorage;
    use pie_agent_core::harness::session::session::{Session, SessionStorage};
    use std::sync::Arc;

    use super::*;
    use stead_brain_protocol::{ModelSelection, TabContext, ToolResultPayload};

    async fn initialized(temp: &tempfile::TempDir) -> BrainCore {
        let (core, _) = BrainCore::initialize(InitializeParams {
            app_support_dir: Some(temp.path().join("Stead")),
            approved_roots: vec![temp.path().join("approved")],
            dev_allow_config_files: false,
        })
        .await
        .unwrap();
        core
    }

    fn spawn_http_response(body: String, content_type: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let content_type = content_type.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        format!("http://{addr}/fixture")
    }

    #[tokio::test]
    async fn creates_lists_loads_and_appends_session() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        assert!(core.config().agent_root().join("AGENTS.md").is_file());
        assert!(core.config().agent_root().join("SOUL.md").is_file());

        let created = core
            .create_session(
                "r1".to_string(),
                CreateSessionParams {
                    title: Some("First".to_string()),
                    origin_surface: Some("sidebar".to_string()),
                },
            )
            .await
            .unwrap();
        let BrainEvent::SessionCreated { session } = &created[0].event else {
            panic!("expected session_created");
        };

        let sent = core
            .send_message(
                "r2".to_string(),
                SendMessageParams {
                    session_id: session.id.clone(),
                    text: "hello".to_string(),
                    tab_context: Some(TabContext {
                        tab_id: 7,
                        url: "https://example.com".to_string(),
                        title: "Example".to_string(),
                    }),
                    model: Some(ModelSelection {
                        provider: "faux".to_string(),
                        model: "faux".to_string(),
                    }),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            sent.last().unwrap().event,
            BrainEvent::AssistantDone(_)
        ));

        let listed = core.list_sessions("r3".to_string()).await.unwrap();
        let BrainEvent::Sessions { sessions } = &listed[0].event else {
            panic!("expected sessions");
        };
        assert_eq!(sessions.len(), 1);

        let messages = core.session_messages(&session.id).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "[faux] hello");
        assert_eq!(messages[1].metadata["provider"], "faux");
        assert_eq!(messages[1].metadata["model"], "faux");
        assert!(session.path.join("attachments").is_dir());
        assert!(session.path.join("tmp").is_dir());
        assert!(session.path.join("artifacts").is_dir());
    }

    #[tokio::test]
    async fn local_instruction_files_extend_system_prompt() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        fs::write(
            core.config().agent_root().join("AGENTS.md"),
            "Prefer concise native browser actions.",
        )
        .unwrap();
        fs::write(
            core.config().agent_root().join("SOUL.md"),
            "Use a calm product-engineering voice.",
        )
        .unwrap();

        let prompt = core.system_prompt().await.unwrap();
        assert!(prompt.contains("<local_agent_instructions>"));
        assert!(prompt.contains("Prefer concise native browser actions."));
        assert!(prompt.contains("<local_persona_notes>"));
        assert!(prompt.contains("Use a calm product-engineering voice."));
    }

    #[tokio::test]
    async fn memory_tool_persists_searches_injects_and_forgets() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        let tool = MemoryTool::new(Arc::new(core.memory().clone()));

        let saved = tool
            .execute(
                "memory_1",
                json!({
                    "action": "save",
                    "name": "Project Voice",
                    "description": "Preferred tone for Stead work.",
                    "type": "preference",
                    "content": "The user prefers direct, low-fluff engineering prose."
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(saved.details["saved"]["key"], "project-voice");

        let listed = tool
            .execute(
                "memory_2",
                json!({ "action": "list" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(listed.details["memories"][0]["key"], "project-voice");

        let searched = tool
            .execute(
                "memory_3",
                json!({ "action": "search", "query": "low-fluff" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(searched.details["matches"][0]["key"], "project-voice");

        let prompt = core.system_prompt().await.unwrap();
        assert!(prompt.contains("<memory>"));
        assert!(prompt.contains("The user prefers direct, low-fluff engineering prose."));

        let forgotten = tool
            .execute(
                "memory_4",
                json!({ "action": "forget", "name": "Project Voice" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(forgotten.details["forgotten"]["key"], "project-voice");
        assert!(!core.system_prompt().await.unwrap().contains("<memory>"));
    }

    #[tokio::test]
    async fn memory_tool_never_accepts_raw_paths_as_addresses() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        let tool = MemoryTool::new(Arc::new(core.memory().clone()));

        let result = tool
            .execute(
                "memory_path",
                json!({
                    "action": "save",
                    "name": "../secrets/token",
                    "description": "Path-looking names are normalized.",
                    "content": "This stays inside the memory key namespace."
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.details["saved"]["key"], "secrets-token");
        assert!(core.memory().root().join("secrets-token.json").is_file());
        assert!(!core.config().agent_root().join("secrets").exists());
    }

    #[tokio::test]
    async fn ask_user_tool_emits_prompt_and_waits_for_result() {
        let pending: PendingToolResults = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = AskUserTool::new(
            "session_ask".to_string(),
            "request_ask".to_string(),
            pending.clone(),
            tx,
        );

        let handle = tokio::spawn(async move {
            tool.execute(
                "ask_1",
                json!({
                    "prompt": "Pick a path.",
                    "questions": [{
                        "id": "path",
                        "question": "Which path?",
                        "options": [
                            { "label": "Fast", "description": "Move quickly." },
                            { "label": "Careful", "description": "Inspect first." }
                        ]
                    }]
                }),
                CancellationToken::new(),
                None,
            )
            .await
        });

        let status = rx.recv().await.unwrap();
        assert!(matches!(status.event, BrainEvent::ToolStatus(_)));
        let call = rx.recv().await.unwrap();
        let BrainEvent::ToolCall(envelope) = call.event else {
            panic!("expected ask_user tool call");
        };
        assert_eq!(envelope.name, "ask_user");
        assert_eq!(envelope.arguments["prompt"], "Pick a path.");

        let sender = pending.lock().await.remove("session_ask:ask_1").unwrap();
        sender
            .send(ToolResultPayload {
                ok: true,
                content: json!({
                    "answers": [{
                        "id": "path",
                        "selected_labels": ["Careful"],
                        "custom": ""
                    }]
                }),
                error: None,
                tainted: false,
            })
            .unwrap();
        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.details["answers"][0]["id"], "path");
        assert_eq!(
            result.details["answers"][0]["selected_labels"][0],
            "Careful"
        );
    }

    #[tokio::test]
    async fn notification_tool_emits_compact_session_event() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = NotificationTool::new(
            "session_notice".to_string(),
            "request_notice".to_string(),
            tx,
        );
        let long_body = "x".repeat(MAX_NOTIFICATION_BODY_CHARS + 32);
        let result = tool
            .execute(
                "notice_1",
                json!({
                    "title": "Done",
                    "body": long_body,
                    "level": "success",
                    "category": "task"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.details["truncated"], true);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.request_id.as_deref(), Some("request_notice"));
        assert_eq!(event.session_id.as_deref(), Some("session_notice"));
        let BrainEvent::Notification(info) = event.event else {
            panic!("expected notification event");
        };
        assert_eq!(info.title.as_deref(), Some("Done"));
        assert_eq!(info.level.as_deref(), Some("success"));
        assert_eq!(info.category.as_deref(), Some("task"));
        assert_eq!(info.body.chars().count(), MAX_NOTIFICATION_BODY_CHARS);
    }

    #[tokio::test]
    async fn get_time_tool_returns_compact_time_metadata() {
        let tool = GetTimeTool::new();
        let result = tool
            .execute("time_1", json!({}), CancellationToken::new(), None)
            .await
            .unwrap();

        assert_eq!(result.details["source"], "stead-brain-helper");
        assert!(result.details["utc"].as_str().unwrap().contains('T'));
        assert!(result.details["local"].as_str().unwrap().contains('T'));
        assert!(result.details["unix_timestamp"].as_i64().unwrap() > 0);
        assert!(result.details["utc_offset_seconds"].as_i64().is_some());
    }

    #[test]
    fn local_tool_catalog_includes_get_time() {
        assert_eq!(local_tool_names(), vec!["get_time", "WebFetch"]);
        let tools = local_tools();
        assert!(
            tools
                .iter()
                .any(|tool| tool.definition().name == "get_time")
        );
        assert!(
            tools
                .iter()
                .any(|tool| tool.definition().name == "WebFetch")
        );
    }

    #[test]
    fn interactive_tool_catalog_includes_user_prompt_and_notifications() {
        assert_eq!(user_prompt_tool_names(), vec!["ask_user", "notification"]);
    }

    #[tokio::test]
    async fn web_fetch_tool_fetches_http_without_browser_state() {
        let url = spawn_http_response(
            "<html><body>public fixture body</body></html>".to_string(),
            "text/html; charset=utf-8",
        );
        let tool = WebFetchTool::new();
        let result = tool
            .execute(
                "webfetch_1",
                json!({ "url": url }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.details["status"], 200);
        assert_eq!(result.details["ok"], true);
        assert_eq!(result.details["truncated"], false);
        assert!(
            result.details["text"]
                .as_str()
                .unwrap()
                .contains("public fixture body")
        );
        assert_eq!(result.details["content_type"], "text/html; charset=utf-8");
    }

    #[tokio::test]
    async fn web_fetch_tool_rejects_non_http_schemes() {
        let tool = WebFetchTool::new();
        let error = tool
            .execute(
                "webfetch_file",
                json!({ "url": "file:///Users/judekim/.ssh/id_rsa" }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("only supports http/https"));
    }

    #[tokio::test]
    async fn web_fetch_tool_caps_response_bytes() {
        let url = spawn_http_response("abcdefghijklmnopqrstuvwxyz".to_string(), "text/plain");
        let tool = WebFetchTool::new();
        let result = tool
            .execute(
                "webfetch_cap",
                json!({ "url": url, "max_bytes": 12 }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.details["bytes_read"], 12);
        assert_eq!(result.details["byte_cap"], 12);
        assert_eq!(result.details["truncated"], true);
        assert_eq!(result.details["text"], "abcdefghijkl");
    }

    #[tokio::test]
    async fn bundled_stead_skills_load_without_user_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let skills = load_stead_skills(temp.path().join("missing-skills")).await;
        let names: Vec<_> = skills.iter().map(|skill| skill.name.as_str()).collect();

        assert!(names.contains(&"artifact-document"));
        assert!(names.contains(&"browser-credential-handoff"));
        assert!(names.contains(&"gmail-browser"));
        assert!(names.contains(&"github-browser"));
        assert!(names.contains(&"notion-browser"));
        assert!(
            skills
                .iter()
                .all(|skill| skill.source == SkillSource::Builtin)
        );
    }

    #[tokio::test]
    async fn loads_and_invokes_stead_skills_with_pie_catalog_shape() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        let skill_dir = core.config().agent_root().join("skills").join("gmail-flow");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: gmail-flow\ndescription: Use Gmail with native browser tools.\n---\n1. Snapshot the inbox.\n2. Prefer semantic clicks.\n",
        )
        .unwrap();

        let skills = core.load_skills().await;
        let gmail_flow = skills
            .iter()
            .find(|skill| skill.name == "gmail-flow")
            .expect("user skill should load");
        assert_eq!(gmail_flow.source, SkillSource::User);
        assert!(
            skills
                .iter()
                .any(|skill| skill.name == "gmail-browser" && skill.source == SkillSource::Builtin)
        );

        let tool = SkillInvocationTool::new(skills);
        let result = tool
            .execute(
                "skill_1",
                json!({
                    "name": "gmail-flow",
                    "additional_instructions": "Apply only to the current tab."
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.details["name"], "gmail-flow");
        match &result.content[0] {
            pie_ai::UserContentBlock::Text(text) => {
                assert!(text.text.contains("<skill name=\"gmail-flow\""));
                assert!(text.text.contains("Snapshot the inbox"));
                assert!(text.text.contains("Apply only to the current tab"));
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn user_skill_overrides_builtin_by_name() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        let skill_dir = core
            .config()
            .agent_root()
            .join("skills")
            .join("gmail-browser");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: gmail-browser\ndescription: User override for Gmail.\n---\nUser-specific Gmail workflow.\n",
        )
        .unwrap();

        let skills = core.load_skills().await;
        let gmail: Vec<_> = skills
            .iter()
            .filter(|skill| skill.name == "gmail-browser")
            .collect();
        assert_eq!(gmail.len(), 1);
        assert_eq!(gmail[0].source, SkillSource::User);
        assert_eq!(gmail[0].description, "User override for Gmail.");
        assert!(gmail[0].content.contains("User-specific Gmail workflow"));
    }

    #[tokio::test]
    async fn normal_message_requires_explicit_model() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        let created = core
            .create_session("r1".to_string(), CreateSessionParams::default())
            .await
            .unwrap();
        let BrainEvent::SessionCreated { session } = &created[0].event else {
            panic!("expected session_created");
        };

        let err = core
            .send_message(
                "r2".to_string(),
                SendMessageParams {
                    session_id: session.id.clone(),
                    text: "hello".to_string(),
                    tab_context: None,
                    model: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BrainError::ModelNotConfigured));
    }

    #[tokio::test]
    async fn provider_auth_status_never_echoes_secret() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        let events = core
            .set_provider_credential(
                "auth1".to_string(),
                stead_brain_protocol::SetProviderCredentialParams {
                    provider: "anthropic".to_string(),
                    credential: stead_brain_protocol::ProviderCredentialInput::ApiKey {
                        value: "sk-ant-secret".to_string(),
                    },
                },
            )
            .await
            .unwrap();
        let payload = serde_json::to_string(&events).unwrap();
        assert!(payload.contains("anthropic"));
        assert!(!payload.contains("sk-ant-secret"));

        let listed = core.list_provider_auth("auth2".to_string()).await.unwrap();
        let listed_payload = serde_json::to_string(&listed).unwrap();
        assert!(listed_payload.contains("api_key"));
        assert!(!listed_payload.contains("sk-ant-secret"));
    }

    #[tokio::test]
    async fn model_catalog_comes_from_resolvable_pie_models() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;

        let events = core.list_models("models1".to_string()).await.unwrap();
        let BrainEvent::ModelCatalog { providers } = &events[0].event else {
            panic!("expected model_catalog");
        };
        let anthropic = providers
            .iter()
            .find(|provider| provider.provider == "anthropic")
            .expect("anthropic catalog");
        let codex = providers
            .iter()
            .find(|provider| provider.provider == "openai-codex")
            .expect("openai-codex catalog");

        assert!(anthropic.supports_oauth);
        assert!(!anthropic.supports_codex_import);
        assert!(codex.supports_oauth);
        assert!(codex.supports_codex_import);
        assert!(
            anthropic
                .models
                .iter()
                .any(|model| model.id == "claude-opus-4-6")
        );
        assert!(codex.models.iter().any(|model| model.id == "gpt-5.3-codex"));

        for provider in providers {
            for model in provider.models.iter().take(3) {
                assert!(
                    pie_ai::get_model(&pie_ai::Provider(provider.provider.clone()), &model.id)
                        .is_some(),
                    "catalog model must resolve through Pie: {}/{}",
                    provider.provider,
                    model.id
                );
            }
        }
    }

    #[tokio::test]
    async fn model_catalog_includes_auth_status_without_secret() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        core.set_provider_credential(
            "auth1".to_string(),
            stead_brain_protocol::SetProviderCredentialParams {
                provider: "anthropic".to_string(),
                credential: stead_brain_protocol::ProviderCredentialInput::ApiKey {
                    value: "sk-ant-catalog-secret".to_string(),
                },
            },
        )
        .await
        .unwrap();

        let events = core.list_models("models2".to_string()).await.unwrap();
        let payload = serde_json::to_string(&events).unwrap();
        assert!(payload.contains("\"type\":\"model_catalog\""));
        assert!(payload.contains("\"configured\":true"));
        assert!(payload.contains("\"credential_kind\":\"api_key\""));
        assert!(!payload.contains("sk-ant-catalog-secret"));
    }

    #[tokio::test]
    async fn emits_browser_tool_call_for_tool_command() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("approved")).unwrap();
        let core = initialized(&temp).await;
        let created = core
            .create_session("r1".to_string(), CreateSessionParams::default())
            .await
            .unwrap();
        let BrainEvent::SessionCreated { session } = &created[0].event else {
            panic!("expected session_created");
        };

        let events = core
            .send_message(
                "r2".to_string(),
                SendMessageParams {
                    session_id: session.id.clone(),
                    text: "/tool browser.list_tabs {\"active\":true}".to_string(),
                    tab_context: None,
                    model: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(events[1].event, BrainEvent::ToolCall(_)));
    }

    #[tokio::test]
    async fn file_access_rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let approved = temp.path().join("approved");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&approved).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.txt"), approved.join("escape.txt"))
            .unwrap();

        let core = initialized(&temp).await;
        #[cfg(unix)]
        assert!(matches!(
            core.files()
                .target_from_params(
                    &json!({ "path": approved.join("escape.txt") }),
                    "path",
                    false
                )
                .await,
            Err(_)
        ));
    }

    #[tokio::test]
    async fn constructs_pie_harness_options() {
        let storage = Arc::new(MemorySessionStorage::new()) as Arc<dyn SessionStorage>;
        let session = Session::new(storage);
        let options = AgentHarnessOptions::new(build_faux_pie_model(), session);
        assert!(options.model.context_window > 0);
    }

    #[tokio::test]
    async fn browser_tool_adapter_routes_through_bridge() {
        struct FakeBridge;

        #[async_trait]
        impl BrowserToolBridge for FakeBridge {
            async fn call_browser_tool(
                &self,
                tool_call_id: &str,
                name: &str,
                arguments: Value,
                _cancel: CancellationToken,
            ) -> Result<ToolResultPayload> {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(name, "browser.list_tabs");
                assert_eq!(arguments["active"], true);
                Ok(ToolResultPayload {
                    ok: true,
                    content: json!({ "tabs": [] }),
                    error: None,
                    tainted: false,
                })
            }
        }

        let tools = browser_tools(Arc::new(FakeBridge));
        let tool = tools
            .iter()
            .find(|tool| tool.definition().name == "browser.list_tabs")
            .unwrap();
        let result = tool
            .execute(
                "call_1",
                json!({ "active": true }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.details["tabs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn browser_tool_result_converts_screenshot_payload_to_image_block() {
        let (content, details) = browser_tool_result_content(ToolResultPayload {
            ok: true,
            content: json!({
                "result": { "ok": true },
                "mime_type": "image/png",
                "image_base64": "abc123",
                "image_included": true
            }),
            error: None,
            tainted: false,
        });

        assert_eq!(content.len(), 2);
        assert!(details.get("image_base64").is_none());
        assert_eq!(details["image_base64_chars"], 6);
        assert!(matches!(&content[0], pie_ai::UserContentBlock::Text(_)));
        match &content[1] {
            pie_ai::UserContentBlock::Image(image) => {
                assert_eq!(image.data, "abc123");
                assert_eq!(image.mime_type, "image/png");
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[test]
    fn browser_tool_result_keeps_metadata_only_when_image_is_omitted() {
        let (content, details) = browser_tool_result_content(ToolResultPayload {
            ok: true,
            content: json!({
                "result": { "ok": true },
                "image_omitted": true,
                "reason": "Screenshot exceeded the brain stdio image cap."
            }),
            error: None,
            tainted: false,
        });

        assert_eq!(content.len(), 1);
        assert_eq!(details["image_omitted"], true);
        assert!(details.get("image_base64").is_none());
    }

    #[test]
    fn browser_tool_result_withholds_tainted_payloads() {
        let (content, details) = browser_tool_result_content(ToolResultPayload {
            ok: true,
            content: json!({
                "image_base64": "secret",
                "value": "hidden"
            }),
            error: None,
            tainted: true,
        });

        assert_eq!(content.len(), 1);
        assert_eq!(details, json!({ "tainted": true }));
        match &content[0] {
            pie_ai::UserContentBlock::Text(text) => {
                assert!(text.text.contains("tainted"));
                assert!(!text.text.contains("secret"));
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_tool_adapter_enforces_roots() {
        let temp = tempfile::tempdir().unwrap();
        let approved = temp.path().join("approved");
        fs::create_dir_all(&approved).unwrap();
        fs::write(approved.join("note.txt"), "alpha\nbeta").unwrap();
        let core = initialized(&temp).await;

        let tools = file_tools(Arc::new(core.files().clone()));
        let read = tools
            .iter()
            .find(|tool| tool.definition().name == "files.read")
            .unwrap();
        let result = read
            .execute(
                "call_1",
                json!({ "path": approved.join("note.txt") }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.details["content"], "alpha\nbeta");

        let denied = read
            .execute(
                "call_2",
                json!({ "path": temp.path().join("outside.txt") }),
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(denied.is_err());

        let created = core
            .create_session("r1".to_string(), CreateSessionParams::default())
            .await
            .unwrap();
        let BrainEvent::SessionCreated { session } = &created[0].event else {
            panic!("expected session_created");
        };

        let write = tools
            .iter()
            .find(|tool| tool.definition().name == "files.write")
            .unwrap();
        let written = write
            .execute(
                "call_3",
                json!({
                    "root": "session_tmp",
                    "session_id": session.id,
                    "path": "preview.html",
                    "content": "<p>preview</p>"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let written_path = written.details["path"].as_str().unwrap();
        assert!(written_path.ends_with("/tmp/preview.html"));

        let session_tools =
            file_tools_for_session(Arc::new(core.files().clone()), Some(session.id.clone()));
        let session_write = session_tools
            .iter()
            .find(|tool| tool.definition().name == "files.write")
            .unwrap();
        let implicit = session_write
            .execute(
                "call_4",
                json!({
                    "root": "session_tmp",
                    "path": "implicit-session.txt",
                    "content": "current session is supplied by the tool wrapper"
                }),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let implicit_path = implicit.details["path"].as_str().unwrap();
        assert!(implicit_path.ends_with("/tmp/implicit-session.txt"));
    }

    #[test]
    fn parses_tool_command() {
        let (name, args) = parse_tool_command("/tool browser.snapshot {\"tab_id\":1}").unwrap();
        assert_eq!(name, "browser.snapshot");
        assert_eq!(args["tab_id"], 1);
        assert!(parse_tool_command("normal message").is_none());
    }
}
