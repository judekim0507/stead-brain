use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(flatten)]
    pub message: BrowserRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserRequest {
    Initialize(InitializeParams),
    CreateSession(CreateSessionParams),
    ListSessions,
    LoadSession { session_id: String },
    SendMessage(SendMessageParams),
    CancelTurn { session_id: String },
    ToolResult(ToolResultEnvelope),
    ListModels,
    ListProviderAuth,
    SetProviderCredential(SetProviderCredentialParams),
    StartProviderOAuth(StartProviderOAuthParams),
    ImportCodexAuth(ImportCodexAuthParams),
    ClearProviderCredential { provider: String },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_support_dir: Option<PathBuf>,
    #[serde(default)]
    pub file_access_mode: FileAccessMode,
    #[serde(default)]
    pub approved_roots: Vec<PathBuf>,
    #[serde(default)]
    pub dev_allow_config_files: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccessMode {
    #[default]
    SessionOnly,
    ApprovedRoots,
    FullDisk,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionMode {
    Ask,
    #[default]
    Read,
    Full,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_surface: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SendMessageParams {
    pub session_id: String,
    pub text: String,
    #[serde(default)]
    pub tab_context: Option<TabContext>,
    #[serde(default)]
    pub model: Option<ModelSelection>,
    #[serde(default)]
    pub permission_mode: AgentPermissionMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetProviderCredentialParams {
    pub provider: String,
    pub credential: ProviderCredentialInput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderCredentialInput {
    ApiKey {
        value: String,
    },
    OAuth {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_unix_secs: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartProviderOAuthParams {
    pub provider: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportCodexAuthParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabContext {
    pub tab_id: i32,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub event: BrainEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrainEvent {
    Ready(ReadyInfo),
    SessionCreated {
        session: SessionInfo,
    },
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    SessionLoaded {
        session: SessionInfo,
    },
    AssistantDelta {
        text: String,
    },
    AssistantDone(AssistantDone),
    ToolCall(ToolCallEnvelope),
    ToolStatus(ToolStatus),
    ModelCatalog {
        providers: Vec<ModelCatalogProvider>,
    },
    ProviderAuthStatus {
        providers: Vec<ProviderAuthStatus>,
    },
    ProviderAuthUrl(ProviderAuthUrl),
    ProviderAuthCompleted {
        status: ProviderAuthStatus,
    },
    Notification(NotificationInfo),
    Error(ErrorInfo),
    UsageUpdate(UsageUpdate),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyInfo {
    pub brain_version: String,
    pub pie_commit: String,
    pub app_support_dir: PathBuf,
    /// Skill catalog snapshot (names/descriptions only, no skill bodies) so
    /// UI surfaces can offer @-mention discovery without a extra round trip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// "builtin" or "user".
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantDone {
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallEnvelope {
    pub tool_call_id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub tainted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultEnvelope {
    pub session_id: String,
    pub tool_call_id: String,
    pub result: ToolResultPayload,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultPayload {
    pub ok: bool,
    #[serde(default)]
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub tainted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStatus {
    pub tool_call_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthStatus {
    pub provider: String,
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_secs: Option<i64>,
    #[serde(default)]
    pub needs_refresh: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAuthUrl {
    pub provider: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationInfo {
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogProvider {
    pub provider: String,
    pub label: String,
    #[serde(default)]
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub supports_oauth: bool,
    #[serde(default)]
    pub supports_codex_import: bool,
    pub models: Vec<ModelCatalogEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub name: String,
    pub api: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub input: Vec<String>,
    pub context_window: u32,
    pub max_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageUpdate {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl ResponseEnvelope {
    pub fn event(request_id: impl Into<Option<String>>, event: BrainEvent) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            session_id: None,
            event,
        }
    }

    pub fn session_event(
        request_id: impl Into<Option<String>>,
        session_id: impl Into<String>,
        event: BrainEvent,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            session_id: Some(session_id.into()),
            event,
        }
    }
}

pub fn parse_request_line(line: &str) -> Result<RequestEnvelope, serde_json::Error> {
    serde_json::from_str(line)
}

pub fn encode_response_line(response: &ResponseEnvelope) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let request = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: "r1".into(),
            message: BrowserRequest::CreateSession(CreateSessionParams {
                title: Some("Trip".into()),
                origin_surface: Some("sidebar".into()),
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        let decoded: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
        assert!(json.contains("\"type\":\"create_session\""));
    }

    #[test]
    fn response_line_is_newline_framed() {
        let response = ResponseEnvelope::event(
            Some("r1".into()),
            BrainEvent::Ready(ReadyInfo {
                brain_version: "0.1.0".into(),
                pie_commit: "abc".into(),
                app_support_dir: "/tmp/stead".into(),
                skills: vec![],
            }),
        );
        let encoded = encode_response_line(&response).unwrap();
        assert!(encoded.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<ResponseEnvelope>(encoded.trim_end()).unwrap(),
            response
        );
    }

    #[test]
    fn provider_auth_messages_do_not_require_secret_echo() {
        let request = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: "auth1".into(),
            message: BrowserRequest::SetProviderCredential(SetProviderCredentialParams {
                provider: "anthropic".into(),
                credential: ProviderCredentialInput::ApiKey {
                    value: "sk-secret".into(),
                },
            }),
        };
        let decoded: RequestEnvelope =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(decoded, request);

        let status = ProviderAuthStatus {
            provider: "anthropic".into(),
            configured: true,
            credential_kind: Some("api_key".into()),
            source: Some("manual".into()),
            account_id: None,
            expires_at_unix_secs: None,
            needs_refresh: false,
        };
        let response = ResponseEnvelope::event(
            Some("auth1".into()),
            BrainEvent::ProviderAuthCompleted { status },
        );
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"provider\":\"anthropic\""));
        assert!(!json.contains("sk-secret"));
    }
}
