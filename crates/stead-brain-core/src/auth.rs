use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stead_brain_protocol::{
    ProviderAuthStatus, ProviderCredentialInput, ResponseEnvelope, StartProviderOAuthParams,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{BrainError, Result};

const AUTH_VERSION: u32 = 1;
const REFRESH_SKEW_SECS: i64 = 5 * 60;
const ANTHROPIC_EXPIRES_AT_IS_MILLIS: i64 = 1000;
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_ISSUER: &str = "https://auth.openai.com";
const CODEX_TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const CODEX_PRIMARY_PORT: u16 = 1455;
const CODEX_FALLBACK_PORT: u16 = 1457;
const AUTH_STORE_ENV: &str = "STEAD_BRAIN_AUTH_STORE";
const AUTH_STORE_FILE: &str = "file";
const KEYCHAIN_SERVICE: &str = "com.stead.browser.brain.provider-auth";

#[derive(Clone)]
pub struct ProviderAuthStore {
    storage: AuthStorage,
    state: Arc<RwLock<AuthFile>>,
}

#[derive(Clone)]
enum AuthStorage {
    File {
        path: PathBuf,
    },
    #[cfg(target_os = "macos")]
    Keychain {
        service: String,
        account: String,
        legacy_path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct CredentialInjection {
    pub api_key: String,
    pub auth_type: CredentialAuthType,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialAuthType {
    ApiKey,
    OAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthFile {
    version: u32,
    #[serde(default)]
    providers: HashMap<String, StoredProviderCredential>,
}

impl Default for AuthFile {
    fn default() -> Self {
        Self {
            version: AUTH_VERSION,
            providers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredProviderCredential {
    ApiKey {
        value: String,
        #[serde(default)]
        source: CredentialSource,
    },
    OAuth {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_unix_secs: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
        #[serde(default)]
        source: CredentialSource,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CredentialSource {
    #[default]
    Manual,
    OAuth,
    ImportedCodex,
}

impl ProviderAuthStore {
    pub async fn open(agent_root: &Path) -> Result<Self> {
        let auth_dir = agent_root.join("auth");
        tokio::fs::create_dir_all(&auth_dir).await?;
        let legacy_path = auth_dir.join("provider_credentials.json");
        let storage = AuthStorage::new(agent_root, legacy_path);
        let state = storage.load().await?;
        Ok(Self {
            storage,
            state: Arc::new(RwLock::new(state)),
        })
    }

    pub fn statuses(&self) -> Vec<ProviderAuthStatus> {
        let state = self.state.read().expect("auth lock poisoned");
        let mut providers: BTreeSet<String> = [
            "anthropic".to_string(),
            "openai".to_string(),
            "openai-codex".to_string(),
            "google".to_string(),
        ]
        .into_iter()
        .collect();
        providers.extend(state.providers.keys().cloned());
        providers
            .into_iter()
            .map(|provider| {
                let stored = state.providers.get(&provider);
                let (credential_kind, source, account_id, expires_at_unix_secs) = match stored {
                    Some(StoredProviderCredential::ApiKey { source, .. }) => (
                        Some("api_key".to_string()),
                        Some(source.as_str().to_string()),
                        None,
                        None,
                    ),
                    Some(StoredProviderCredential::OAuth {
                        source,
                        account_id,
                        expires_at_unix_secs,
                        ..
                    }) => (
                        Some("oauth".to_string()),
                        Some(source.as_str().to_string()),
                        account_id.clone(),
                        *expires_at_unix_secs,
                    ),
                    None => (None, None, None, None),
                };
                ProviderAuthStatus {
                    provider,
                    configured: stored.is_some(),
                    credential_kind,
                    source,
                    account_id,
                    expires_at_unix_secs,
                    needs_refresh: expires_at_unix_secs
                        .map(|exp| exp <= now_secs() + REFRESH_SKEW_SECS)
                        .unwrap_or(false),
                }
            })
            .collect()
    }

    pub async fn set_credential(
        &self,
        provider: String,
        credential: ProviderCredentialInput,
    ) -> Result<ProviderAuthStatus> {
        let normalized = normalize_provider(&provider);
        let stored = match credential {
            ProviderCredentialInput::ApiKey { value } => {
                if value.trim().is_empty() {
                    return Err(BrainError::InvalidRequest(
                        "provider API key must not be empty".to_string(),
                    ));
                }
                StoredProviderCredential::ApiKey {
                    value,
                    source: CredentialSource::Manual,
                }
            }
            ProviderCredentialInput::OAuth {
                access_token,
                refresh_token,
                expires_at_unix_secs,
                account_id,
            } => {
                if access_token.trim().is_empty() {
                    return Err(BrainError::InvalidRequest(
                        "provider OAuth access token must not be empty".to_string(),
                    ));
                }
                StoredProviderCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires_at_unix_secs,
                    account_id,
                    source: CredentialSource::Manual,
                }
            }
        };
        self.set_stored_credential(normalized.clone(), stored)
            .await?;
        Ok(self
            .statuses()
            .into_iter()
            .find(|status| status.provider == normalized)
            .expect("status exists after set"))
    }

    pub async fn clear(&self, provider: &str) -> Result<Vec<ProviderAuthStatus>> {
        let normalized = normalize_provider(provider);
        {
            let mut state = self.state.write().expect("auth lock poisoned");
            state.providers.remove(&normalized);
        }
        self.save().await?;
        Ok(self.statuses())
    }

    pub async fn import_codex_auth(&self, path: Option<PathBuf>) -> Result<ProviderAuthStatus> {
        let path = path.unwrap_or_else(default_codex_auth_path);
        let text = tokio::fs::read_to_string(&path).await.map_err(|error| {
            BrainError::InvalidRequest(format!("read Codex auth file: {error}"))
        })?;
        let auth: CodexAuthFile = serde_json::from_str(&text).map_err(|error| {
            BrainError::InvalidRequest(format!("parse Codex auth file {}: {error}", path.display()))
        })?;
        let tokens = auth.tokens.ok_or_else(|| {
            BrainError::InvalidRequest("Codex auth file did not contain tokens".to_string())
        })?;
        if tokens.access_token.trim().is_empty() {
            return Err(BrainError::InvalidRequest(
                "Codex auth file access token was empty".to_string(),
            ));
        }
        let account_id = tokens.account_id.or_else(|| {
            tokens
                .id_token
                .as_deref()
                .and_then(chatgpt_account_id_from_jwt)
        });
        let expires_at_unix_secs = jwt_exp_secs(&tokens.access_token);
        self.set_stored_credential(
            "openai-codex".to_string(),
            StoredProviderCredential::OAuth {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires_at_unix_secs,
                account_id,
                source: CredentialSource::ImportedCodex,
            },
        )
        .await?;
        Ok(self
            .statuses()
            .into_iter()
            .find(|status| status.provider == "openai-codex")
            .expect("Codex status exists after import"))
    }

    pub async fn start_oauth(
        &self,
        request_id: String,
        params: StartProviderOAuthParams,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
    ) -> Result<()> {
        match normalize_provider(&params.provider).as_str() {
            "anthropic" => self.start_anthropic_oauth(request_id, tx).await,
            "openai-codex" => self.start_codex_oauth(request_id, tx).await,
            provider => Err(BrainError::InvalidRequest(format!(
                "OAuth is not supported for provider: {provider}"
            ))),
        }
    }

    pub async fn prepare_model_credential(&self, model: &pie_ai::Model) -> Result<()> {
        let provider = normalize_provider(&model.provider.0);
        if env_credential(&provider).is_some() {
            return Ok(());
        }
        let stored = {
            self.state
                .read()
                .expect("auth lock poisoned")
                .providers
                .get(&provider)
                .cloned()
        };
        let Some(StoredProviderCredential::OAuth {
            access_token,
            refresh_token,
            expires_at_unix_secs,
            account_id,
            source,
        }) = stored
        else {
            return Ok(());
        };

        let Some(expires_at) = expires_at_unix_secs else {
            return Ok(());
        };
        if expires_at > now_secs() + REFRESH_SKEW_SECS {
            return Ok(());
        }
        let Some(refresh_token) = refresh_token else {
            if expires_at <= now_secs() {
                return Err(BrainError::ProviderAuth(format!(
                    "{provider} OAuth credential is expired and has no refresh token"
                )));
            }
            return Ok(());
        };

        let refreshed = match provider.as_str() {
            "anthropic" => {
                let creds = pie_ai::oauth::anthropic::refresh(&pie_ai::oauth::OAuthCredentials {
                    access_token,
                    refresh_token: Some(refresh_token),
                    expires_at: Some(expires_at * ANTHROPIC_EXPIRES_AT_IS_MILLIS),
                    extra: None,
                })
                .await
                .map_err(BrainError::ProviderAuth)?;
                StoredProviderCredential::OAuth {
                    access_token: creds.access_token,
                    refresh_token: creds.refresh_token,
                    expires_at_unix_secs: creds.expires_at.map(|ms| ms / 1000),
                    account_id,
                    source,
                }
            }
            "openai-codex" => {
                let creds = refresh_codex_access_token(&refresh_token).await?;
                StoredProviderCredential::OAuth {
                    expires_at_unix_secs: jwt_exp_secs(&creds.access_token),
                    account_id: creds.account_id.or(account_id).or_else(|| {
                        creds
                            .id_token
                            .as_deref()
                            .and_then(chatgpt_account_id_from_jwt)
                    }),
                    access_token: creds.access_token,
                    refresh_token: creds.refresh_token,
                    source,
                }
            }
            _ => return Ok(()),
        };
        self.set_stored_credential(provider, refreshed).await?;
        Ok(())
    }

    pub fn credential_for_model(&self, model: &pie_ai::Model) -> Option<CredentialInjection> {
        let provider = normalize_provider(&model.provider.0);
        if let Some(api_key) = env_credential(&provider) {
            return Some(CredentialInjection {
                api_key,
                auth_type: CredentialAuthType::ApiKey,
                account_id: env_account_id(&provider),
            });
        }
        let state = self.state.read().expect("auth lock poisoned");
        match state.providers.get(&provider)? {
            StoredProviderCredential::ApiKey { value, .. } => Some(CredentialInjection {
                api_key: value.clone(),
                auth_type: CredentialAuthType::ApiKey,
                account_id: None,
            }),
            StoredProviderCredential::OAuth {
                access_token,
                account_id,
                ..
            } => Some(CredentialInjection {
                api_key: access_token.clone(),
                auth_type: CredentialAuthType::OAuth,
                account_id: account_id.clone(),
            }),
        }
    }

    async fn start_anthropic_oauth(
        &self,
        request_id: String,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
    ) -> Result<()> {
        let auth = self.clone();
        let tx_for_url = tx.clone();
        let request_for_url = request_id.clone();
        let creds = pie_ai::oauth::anthropic::login(pie_ai::oauth::anthropic::LoginCallbacks {
            open_url: Box::new(move |url| {
                emit_auth_url(&tx_for_url, &request_for_url, "anthropic", url);
            }),
        })
        .await
        .map_err(BrainError::ProviderAuth)?;
        auth.set_stored_credential(
            "anthropic".to_string(),
            StoredProviderCredential::OAuth {
                access_token: creds.access_token,
                refresh_token: creds.refresh_token,
                expires_at_unix_secs: creds.expires_at.map(|ms| ms / 1000),
                account_id: None,
                source: CredentialSource::OAuth,
            },
        )
        .await?;
        emit_auth_completed(&tx, &request_id, self.status_for("anthropic"));
        Ok(())
    }

    async fn start_codex_oauth(
        &self,
        request_id: String,
        tx: mpsc::UnboundedSender<ResponseEnvelope>,
    ) -> Result<()> {
        let login = CodexOAuthLogin::new().await?;
        emit_auth_url(&tx, &request_id, "openai-codex", &login.authorize_url);
        let redirect_uri = login.redirect_uri.clone();
        let verifier = login.verifier.clone();
        let state = login.state.clone();
        let callback = login.wait_for_callback().await?;
        if callback.state != state {
            return Err(BrainError::ProviderAuth(
                "Codex OAuth state mismatch".to_string(),
            ));
        }
        let tokens =
            exchange_codex_authorization_code(&callback.code, &redirect_uri, &verifier).await?;
        self.set_stored_credential(
            "openai-codex".to_string(),
            StoredProviderCredential::OAuth {
                expires_at_unix_secs: jwt_exp_secs(&tokens.access_token),
                account_id: tokens.account_id.or_else(|| {
                    tokens
                        .id_token
                        .as_deref()
                        .and_then(chatgpt_account_id_from_jwt)
                }),
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                source: CredentialSource::OAuth,
            },
        )
        .await?;
        emit_auth_completed(&tx, &request_id, self.status_for("openai-codex"));
        Ok(())
    }

    async fn set_stored_credential(
        &self,
        provider: String,
        credential: StoredProviderCredential,
    ) -> Result<()> {
        {
            let mut state = self.state.write().expect("auth lock poisoned");
            state.providers.insert(provider, credential);
        }
        self.save().await
    }

    async fn save(&self) -> Result<()> {
        let state = {
            let state = self.state.read().expect("auth lock poisoned");
            state.clone()
        };
        self.storage.save(&state).await
    }

    fn status_for(&self, provider: &str) -> ProviderAuthStatus {
        let provider = normalize_provider(provider);
        self.statuses()
            .into_iter()
            .find(|status| status.provider == provider)
            .expect("known provider status exists")
    }
}

impl AuthStorage {
    fn new(agent_root: &Path, legacy_path: PathBuf) -> Self {
        if use_file_auth_store() {
            return Self::File { path: legacy_path };
        }

        #[cfg(target_os = "macos")]
        {
            return Self::Keychain {
                service: KEYCHAIN_SERVICE.to_string(),
                account: keychain_account_for_agent_root(agent_root),
                legacy_path,
            };
        }

        #[cfg(not(target_os = "macos"))]
        {
            Self::File { path: legacy_path }
        }
    }

    async fn load(&self) -> Result<AuthFile> {
        match self {
            Self::File { path } => read_auth_file(path).await,
            #[cfg(target_os = "macos")]
            Self::Keychain {
                service,
                account,
                legacy_path,
            } => {
                if let Some(text) = keychain_get(service.clone(), account.clone()).await? {
                    return Ok(serde_json::from_str(&text)?);
                }

                let state = read_auth_file(legacy_path).await?;
                if !state.providers.is_empty() {
                    keychain_set(
                        service.clone(),
                        account.clone(),
                        serde_json::to_string_pretty(&state)?,
                    )
                    .await?;
                    remove_auth_file(legacy_path).await?;
                }
                Ok(state)
            }
        }
    }

    async fn save(&self, state: &AuthFile) -> Result<()> {
        match self {
            Self::File { path } => write_auth_file(path, state).await,
            #[cfg(target_os = "macos")]
            Self::Keychain {
                service,
                account,
                legacy_path,
            } => {
                if state.providers.is_empty() {
                    keychain_delete(service.clone(), account.clone()).await?;
                } else {
                    keychain_set(
                        service.clone(),
                        account.clone(),
                        serde_json::to_string_pretty(state)?,
                    )
                    .await?;
                }
                remove_auth_file(legacy_path).await?;
                Ok(())
            }
        }
    }
}

impl CredentialSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::OAuth => "oauth",
            Self::ImportedCodex => "imported_codex",
        }
    }
}

fn use_file_auth_store() -> bool {
    cfg!(test)
        || std::env::var(AUTH_STORE_ENV)
            .ok()
            .map(|value| {
                let normalized = value.trim().to_ascii_lowercase();
                normalized == AUTH_STORE_FILE || normalized == "json"
            })
            .unwrap_or(false)
}

struct CodexOAuthLogin {
    listener: tokio::net::TcpListener,
    authorize_url: String,
    redirect_uri: String,
    verifier: String,
    state: String,
}

struct CodexCallback {
    code: String,
    state: String,
}

impl CodexOAuthLogin {
    async fn new() -> Result<Self> {
        let (listener, port) = bind_codex_listener().await?;
        let pkce = pie_ai::oauth::pkce::generate_pkce();
        let state = Uuid::new_v4().to_string();
        let redirect_uri = format!("http://localhost:{port}/auth/callback");
        let authorize_url = build_codex_authorize_url(&redirect_uri, &pkce.challenge, &state);
        Ok(Self {
            listener,
            authorize_url,
            redirect_uri,
            verifier: pkce.verifier,
            state,
        })
    }

    async fn wait_for_callback(self) -> Result<CodexCallback> {
        let accept = async {
            let (mut socket, _) = self.listener.accept().await.map_err(|error| {
                BrainError::ProviderAuth(format!("Codex OAuth accept: {error}"))
            })?;
            let mut buf = vec![0u8; 8192];
            let n = socket
                .read(&mut buf)
                .await
                .map_err(|error| BrainError::ProviderAuth(format!("Codex OAuth read: {error}")))?;
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");
            let (code, state) = parse_callback_query(path);
            let body = if code.is_some() && state.is_some() {
                "<html><body><h1>Stead sign-in complete. You can close this tab.</h1></body></html>"
            } else {
                "<html><body><h1>Stead sign-in failed.</h1></body></html>"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            Ok(CodexCallback {
                code: code.ok_or_else(|| {
                    BrainError::ProviderAuth("Codex OAuth callback missing code".to_string())
                })?,
                state: state.ok_or_else(|| {
                    BrainError::ProviderAuth("Codex OAuth callback missing state".to_string())
                })?,
            })
        };
        tokio::time::timeout(Duration::from_secs(5 * 60), accept)
            .await
            .map_err(|_| BrainError::ProviderAuth("Codex OAuth callback timed out".to_string()))?
    }
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    #[serde(default)]
    tokens: Option<CodexAuthTokens>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

struct CodexTokenBundle {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    account_id: Option<String>,
}

async fn bind_codex_listener() -> Result<(tokio::net::TcpListener, u16)> {
    for port in [CODEX_PRIMARY_PORT, CODEX_FALLBACK_PORT] {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok((listener, port)),
            Err(_) => continue,
        }
    }
    Err(BrainError::ProviderAuth(
        "could not bind a local Codex OAuth callback port".to_string(),
    ))
}

fn build_codex_authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", redirect_uri),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "pi"),
    ];
    let query = query
        .iter()
        .map(|(key, value)| format!("{key}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{CODEX_ISSUER}/oauth/authorize?{query}")
}

async fn exchange_codex_authorization_code(
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<CodexTokenBundle> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| BrainError::ProviderAuth(format!("Codex OAuth HTTP client: {error}")))?;
    let response = client
        .post(CODEX_TOKEN_ENDPOINT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CODEX_CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|error| BrainError::ProviderAuth(format!("Codex token exchange: {error}")))?;
    parse_codex_token_response(response).await
}

async fn refresh_codex_access_token(refresh_token: &str) -> Result<CodexTokenBundle> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| BrainError::ProviderAuth(format!("Codex OAuth HTTP client: {error}")))?;
    let response = client
        .post(CODEX_TOKEN_ENDPOINT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CODEX_CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|error| BrainError::ProviderAuth(format!("Codex token refresh: {error}")))?;
    parse_codex_token_response(response).await
}

async fn parse_codex_token_response(response: reqwest::Response) -> Result<CodexTokenBundle> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| BrainError::ProviderAuth(format!("Codex token response: {error}")))?;
    if !status.is_success() {
        return Err(BrainError::ProviderAuth(format!(
            "Codex token endpoint returned {status}: {}",
            truncate_for_error(&text)
        )));
    }
    let parsed: CodexTokenResponse = serde_json::from_str(&text).map_err(|error| {
        BrainError::ProviderAuth(format!("Codex token response was invalid JSON: {error}"))
    })?;
    let account_id = parsed
        .id_token
        .as_deref()
        .and_then(chatgpt_account_id_from_jwt);
    Ok(CodexTokenBundle {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
        account_id,
    })
}

fn env_credential(provider: &str) -> Option<String> {
    match provider {
        "openai-codex" => std::env::var("CODEX_AUTH_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| pie_ai::env_api_keys::get_env_api_key("openai-codex")),
        other => pie_ai::env_api_keys::get_env_api_key(other),
    }
}

fn env_account_id(provider: &str) -> Option<String> {
    match provider {
        "openai-codex" => std::env::var("CODEX_ACCOUNT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        _ => None,
    }
}

fn normalize_provider(provider: &str) -> String {
    match provider {
        "codex" | "chatgpt" | "chatgpt-codex" | "openai_codex" => "openai-codex".to_string(),
        "claude" => "anthropic".to_string(),
        other => other.to_string(),
    }
}

fn default_codex_auth_path() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join("auth.json");
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".codex").join("auth.json")
}

fn now_secs() -> i64 {
    Utc::now().timestamp()
}

fn jwt_exp_secs(jwt: &str) -> Option<i64> {
    jwt_payload(jwt).and_then(|payload| payload.get("exp").and_then(Value::as_i64))
}

fn chatgpt_account_id_from_jwt(jwt: &str) -> Option<String> {
    let payload = jwt_payload(jwt)?;
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            payload
                .get("chatgpt_account_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
}

fn jwt_payload(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn parse_callback_query(path: &str) -> (Option<String>, Option<String>) {
    let query = match path.split_once('?') {
        Some((_, query)) => query,
        None => return (None, None),
    };
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            let decoded = percent_decode(value);
            match key {
                "code" => code = Some(decoded),
                "state" => state = Some(decoded),
                _ => {}
            }
        }
    }
    (code, state)
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn read_auth_file(path: &Path) -> Result<AuthFile> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) if !text.trim().is_empty() => Ok(serde_json::from_str(&text)?),
        Ok(_) => Ok(AuthFile::default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AuthFile::default()),
        Err(error) => Err(error.into()),
    }
}

async fn write_auth_file(path: &Path, state: &AuthFile) -> Result<()> {
    let text = serde_json::to_string_pretty(state)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, text).await?;
    set_private_file_permissions(&tmp).await?;
    tokio::fs::rename(&tmp, path).await?;
    set_private_file_permissions(path).await?;
    Ok(())
}

async fn remove_auth_file(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn keychain_account_for_agent_root(agent_root: &Path) -> String {
    let canonical = std::fs::canonicalize(agent_root).unwrap_or_else(|_| agent_root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!("agents-main-{}", hex_lower(&digest[..16]))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(target_os = "macos")]
async fn keychain_get(service: String, account: String) -> Result<Option<String>> {
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(&service, &account).map_err(keychain_error)?;
        match entry.get_password() {
            Ok(text) => Ok(Some(text)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    })
    .await
    .map_err(|error| BrainError::ProviderAuth(format!("Keychain read task failed: {error}")))?
}

#[cfg(target_os = "macos")]
async fn keychain_set(service: String, account: String, text: String) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(&service, &account).map_err(keychain_error)?;
        entry.set_password(&text).map_err(keychain_error)
    })
    .await
    .map_err(|error| BrainError::ProviderAuth(format!("Keychain write task failed: {error}")))?
}

#[cfg(target_os = "macos")]
async fn keychain_delete(service: String, account: String) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(&service, &account).map_err(keychain_error)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(keychain_error(error)),
        }
    })
    .await
    .map_err(|error| BrainError::ProviderAuth(format!("Keychain delete task failed: {error}")))?
}

#[cfg(target_os = "macos")]
fn keychain_error(error: keyring::Error) -> BrainError {
    BrainError::ProviderAuth(format!("macOS Keychain credential store: {error}"))
}

async fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(path, permissions).await?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn emit_auth_url(
    tx: &mpsc::UnboundedSender<ResponseEnvelope>,
    request_id: &str,
    provider: &str,
    url: &str,
) {
    let _ = tx.send(ResponseEnvelope::event(
        Some(request_id.to_string()),
        stead_brain_protocol::BrainEvent::ProviderAuthUrl(stead_brain_protocol::ProviderAuthUrl {
            provider: provider.to_string(),
            url: url.to_string(),
            expires_in_secs: Some(5 * 60),
        }),
    ));
}

fn emit_auth_completed(
    tx: &mpsc::UnboundedSender<ResponseEnvelope>,
    request_id: &str,
    status: ProviderAuthStatus,
) {
    let _ = tx.send(ResponseEnvelope::event(
        Some(request_id.to_string()),
        stead_brain_protocol::BrainEvent::ProviderAuthCompleted { status },
    ));
}

fn truncate_for_error(text: &str) -> String {
    text.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fake_jwt(payload: Value) -> String {
        let header =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#.as_bytes());
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn codex_authorize_url_contains_pkce_redirect_and_scope() {
        let url = build_codex_authorize_url(
            "http://localhost:1455/auth/callback",
            "challenge-123",
            "state-xyz",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("code_challenge=challenge-123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-xyz"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
    }

    #[test]
    fn codex_callback_query_decodes_code_and_state() {
        let (code, state) = parse_callback_query("/auth/callback?code=abc%2Fdef&state=s+1");
        assert_eq!(code.as_deref(), Some("abc/def"));
        assert_eq!(state.as_deref(), Some("s 1"));
    }

    #[tokio::test]
    async fn stores_api_key_without_exposing_secret_in_status() {
        let dir = TempDir::new().unwrap();
        let auth = ProviderAuthStore::open(dir.path()).await.unwrap();
        let status = auth
            .set_credential(
                "anthropic".to_string(),
                ProviderCredentialInput::ApiKey {
                    value: "sk-ant-test".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(status.provider, "anthropic");
        assert_eq!(status.credential_kind.as_deref(), Some("api_key"));
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("sk-ant-test")
        );
    }

    #[tokio::test]
    async fn test_file_backend_persists_credentials_without_status_echo() {
        let dir = TempDir::new().unwrap();
        let auth = ProviderAuthStore::open(dir.path()).await.unwrap();
        auth.set_credential(
            "anthropic".to_string(),
            ProviderCredentialInput::ApiKey {
                value: "sk-ant-persisted".to_string(),
            },
        )
        .await
        .unwrap();

        let reopened = ProviderAuthStore::open(dir.path()).await.unwrap();
        let status = reopened.status_for("anthropic");
        assert!(status.configured);
        assert_eq!(status.credential_kind.as_deref(), Some("api_key"));
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("sk-ant-persisted")
        );
    }

    #[tokio::test]
    async fn imports_codex_auth_json_without_leaking_tokens() {
        let dir = TempDir::new().unwrap();
        let auth_path = dir.path().join("codex-auth.json");
        let access = fake_jwt(serde_json::json!({ "exp": now_secs() + 3600 }));
        let id = fake_jwt(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_123" }
        }));
        tokio::fs::write(
            &auth_path,
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": access,
                    "refresh_token": "rt-secret-xyz",
                    "id_token": id
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        let auth = ProviderAuthStore::open(dir.path()).await.unwrap();
        let status = auth.import_codex_auth(Some(auth_path)).await.unwrap();
        assert_eq!(status.provider, "openai-codex");
        assert_eq!(status.account_id.as_deref(), Some("acct_123"));
        let status_json = serde_json::to_string(&status).unwrap();
        assert!(!status_json.contains("rt-secret-xyz"));
    }

    #[tokio::test]
    async fn credential_for_codex_injects_account_id() {
        let dir = TempDir::new().unwrap();
        let auth = ProviderAuthStore::open(dir.path()).await.unwrap();
        auth.set_credential(
            "openai-codex".to_string(),
            ProviderCredentialInput::OAuth {
                access_token: "access".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires_at_unix_secs: Some(now_secs() + 3600),
                account_id: Some("acct_123".to_string()),
            },
        )
        .await
        .unwrap();
        let model = pie_ai::Model {
            id: "gpt-5-codex".to_string(),
            name: "Codex".to_string(),
            api: pie_ai::Api::from("openai-codex-responses"),
            provider: pie_ai::Provider::from("openai-codex"),
            base_url: String::new(),
            input: vec![],
            cost: pie_ai::ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            reasoning: true,
            thinking_level_map: Default::default(),
            headers: None,
            compat: None,
        };
        let credential = auth.credential_for_model(&model).unwrap();
        assert_eq!(credential.api_key, "access");
        assert_eq!(credential.auth_type, CredentialAuthType::OAuth);
        assert_eq!(credential.account_id.as_deref(), Some("acct_123"));
    }
}
