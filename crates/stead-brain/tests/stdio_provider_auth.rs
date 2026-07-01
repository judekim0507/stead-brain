use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

struct Helper {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Helper {
    async fn start(app_support_dir: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_stead-brain"))
            .env("STEAD_BRAIN_AUTH_STORE", "file")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("CLAUDE_API_KEY")
            .env_remove("CODEX_AUTH_TOKEN")
            .env_remove("CODEX_ACCOUNT_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn stead-brain helper");
        let stdin = child.stdin.take().expect("helper stdin");
        let stdout = child.stdout.take().expect("helper stdout");
        let mut helper = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
        };
        helper
            .send(json!({
                "protocol_version": 1,
                "request_id": "init",
                "type": "initialize",
                "app_support_dir": app_support_dir,
                "file_access_mode": "session_only",
                "approved_roots": [],
                "dev_allow_config_files": true
            }))
            .await;
        let ready = helper.next_event().await;
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["request_id"], "init");
        helper
    }

    async fn send(&mut self, request: Value) {
        let mut encoded = serde_json::to_vec(&request).expect("encode request");
        encoded.push(b'\n');
        self.stdin.write_all(&encoded).await.expect("write request");
        self.stdin.flush().await.expect("flush request");
    }

    async fn next_event(&mut self) -> Value {
        let line = timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("timed out waiting for helper event")
            .expect("read helper event")
            .expect("helper exited before event");
        serde_json::from_str(&line).expect("decode helper event")
    }

    async fn shutdown(mut self) {
        self.send(json!({
            "protocol_version": 1,
            "request_id": "shutdown",
            "type": "shutdown"
        }))
        .await;
        drop(self.stdin);
        if timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

#[tokio::test]
async fn api_key_auth_over_stdio_never_echoes_secret() {
    let dir = TempDir::new().expect("temp app support");
    let mut helper = Helper::start(dir.path()).await;
    let secret = "sk-ant-stdio-secret";

    helper
        .send(json!({
            "protocol_version": 1,
            "request_id": "set",
            "type": "set_provider_credential",
            "provider": "claude",
            "credential": {
                "kind": "api_key",
                "value": secret
            }
        }))
        .await;
    let completed = helper.next_event().await;
    assert_eq!(completed["type"], "provider_auth_completed");
    assert_eq!(completed["request_id"], "set");
    assert_eq!(completed["status"]["provider"], "anthropic");
    assert_eq!(completed["status"]["credential_kind"], "api_key");
    assert_eq!(completed["status"]["source"], "manual");
    assert!(!completed.to_string().contains(secret));

    helper
        .send(json!({
            "protocol_version": 1,
            "request_id": "list",
            "type": "list_provider_auth"
        }))
        .await;
    let listed = helper.next_event().await;
    assert_eq!(listed["type"], "provider_auth_status");
    assert!(!listed.to_string().contains(secret));
    let anthropic = listed["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["provider"] == "anthropic")
        .expect("anthropic status");
    assert_eq!(anthropic["configured"], true);

    helper.shutdown().await;
}

#[tokio::test]
async fn codex_import_over_stdio_never_echoes_tokens() {
    let dir = TempDir::new().expect("temp app support");
    let codex_auth_path = dir.path().join("codex-auth.json");
    let access = "codex-access-stdio-secret";
    let refresh = "codex-refresh-stdio-secret";
    tokio::fs::write(
        &codex_auth_path,
        json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": access,
                "refresh_token": refresh,
                "account_id": "acct_stdio"
            }
        })
        .to_string(),
    )
    .await
    .expect("write fake Codex auth");

    let mut helper = Helper::start(dir.path()).await;
    helper
        .send(json!({
            "protocol_version": 1,
            "request_id": "import",
            "type": "import_codex_auth",
            "path": codex_auth_path
        }))
        .await;
    let imported = helper.next_event().await;
    assert_eq!(imported["type"], "provider_auth_completed");
    assert_eq!(imported["request_id"], "import");
    assert_eq!(imported["status"]["provider"], "openai-codex");
    assert_eq!(imported["status"]["configured"], true);
    assert_eq!(imported["status"]["credential_kind"], "oauth");
    assert_eq!(imported["status"]["source"], "imported_codex");
    assert_eq!(imported["status"]["account_id"], "acct_stdio");
    let serialized = imported.to_string();
    assert!(!serialized.contains(access));
    assert!(!serialized.contains(refresh));

    helper
        .send(json!({
            "protocol_version": 1,
            "request_id": "list",
            "type": "list_provider_auth"
        }))
        .await;
    let listed = helper.next_event().await;
    let serialized = listed.to_string();
    assert!(!serialized.contains(access));
    assert!(!serialized.contains(refresh));
    let codex = listed["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["provider"] == "openai-codex")
        .expect("Codex status");
    assert_eq!(codex["configured"], true);
    assert_eq!(codex["account_id"], "acct_stdio");

    helper.shutdown().await;
}

#[tokio::test]
async fn model_catalog_over_stdio_uses_real_provider_capabilities() {
    let dir = TempDir::new().expect("temp app support");
    let mut helper = Helper::start(dir.path()).await;

    helper
        .send(json!({
            "protocol_version": 1,
            "request_id": "models",
            "type": "list_models"
        }))
        .await;
    let catalog = helper.next_event().await;
    assert_eq!(catalog["type"], "model_catalog");
    assert_eq!(catalog["request_id"], "models");

    let providers = catalog["providers"].as_array().expect("providers array");
    let anthropic = providers
        .iter()
        .find(|provider| provider["provider"] == "anthropic")
        .expect("anthropic provider");
    let codex = providers
        .iter()
        .find(|provider| provider["provider"] == "openai-codex")
        .expect("codex provider");

    assert_eq!(anthropic["supports_oauth"], true);
    assert_eq!(anthropic["supports_codex_import"], false);
    assert_eq!(codex["supports_oauth"], true);
    assert_eq!(codex["supports_codex_import"], true);
    assert!(
        anthropic["models"]
            .as_array()
            .expect("anthropic models")
            .iter()
            .any(|model| model["id"] == "claude-opus-4-6")
    );
    assert!(
        codex["models"]
            .as_array()
            .expect("codex models")
            .iter()
            .any(|model| model["id"] == "gpt-5.3-codex")
    );

    helper.shutdown().await;
}
