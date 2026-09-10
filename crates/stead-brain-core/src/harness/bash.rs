// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: bind cwd/env/path permissions to a Stead session and clamp timeouts to 120s/600s.

use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use super::PathPolicy;
use super::permission::PermissionPolicy;
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;

pub struct BashTool {
    paths: Arc<PathPolicy>,
    ask: bool,
    permissions: PermissionPolicy,
}

impl BashTool {
    pub fn new(paths: Arc<PathPolicy>, ask: bool) -> Self {
        Self {
            paths,
            ask,
            permissions: PermissionPolicy::default_for_coding_agent(),
        }
    }
}

struct RunOutcome {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    stderr_suffix: Option<String>,
}

#[async_trait]
impl AgentTool for BashTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "bash"
    }

    fn permission_classification(&self, params: &Value) -> PermissionClassification {
        if !self.ask {
            return PermissionClassification::Allow;
        }
        let Some(command) = params.get("command").and_then(Value::as_str) else {
            return PermissionClassification::Allow;
        };
        match self.permissions.prompt_reason(command, &self.paths) {
            Some(reason) => PermissionClassification::Prompt { reason },
            None => PermissionClassification::Allow,
        }
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::from("missing `command`"))?;
        let timeout_secs = requested_timeout_secs(&params);
        let workspace = self
            .paths
            .ensure_workspace()
            .await
            .map_err(|error| AgentToolError::from(error.to_string()))?;
        let outcome = run_with_kill_on_timeout_or_cancel(
            command,
            timeout_secs,
            &workspace,
            self.paths.session_id(),
            &cancel,
        )
        .await?;

        let exit = outcome.exit_code.unwrap_or(-1);
        let (stdout, stdout_truncation) =
            truncate_tail(&outcome.stdout, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut stderr = outcome.stderr;
        if let Some(suffix) = outcome.stderr_suffix {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(&suffix);
        }
        let (stderr, _) = truncate_tail(&stderr, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut text = format!("$ {command}\n");
        if let Some(note) = stdout_truncation.note() {
            text.push_str(&note);
            text.push('\n');
        }
        if !stdout.is_empty() {
            text.push_str(&stdout);
            if !stdout.ends_with('\n') {
                text.push('\n');
            }
        }
        if !stderr.is_empty() {
            text.push_str("[stderr]\n");
            text.push_str(&stderr);
            if !stderr.ends_with('\n') {
                text.push('\n');
            }
        }
        text.push_str(&format!("[exit {exit}]"));
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({ "command": command, "exitCode": exit, "isError": exit != 0 }),
            terminate: None,
        })
    }
}

async fn run_with_kill_on_timeout_or_cancel(
    command: &str,
    timeout_secs: u64,
    workspace: &std::path::Path,
    session_id: &str,
    cancel: &CancellationToken,
) -> Result<RunOutcome, AgentToolError> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if sensitive_environment_name(&name) {
            cmd.env_remove(name);
        }
    }
    cmd.env("STEAD_SESSION", session_id)
        .env("STEAD_WORKSPACE", workspace);

    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|error| AgentToolError::from(format!("spawn: {error}")))?;
    let child_pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain_handle = tokio::spawn(async move {
        let stdout_task = async move {
            let mut value = String::new();
            if let Some(mut pipe) = stdout {
                let _ = pipe.read_to_string(&mut value).await;
            }
            value
        };
        let stderr_task = async move {
            let mut value = String::new();
            if let Some(mut pipe) = stderr {
                let _ = pipe.read_to_string(&mut value).await;
            }
            value
        };
        tokio::join!(stdout_task, stderr_task)
    });

    let (reason, exit_code) = {
        let wait = child.wait();
        tokio::pin!(wait);
        let deadline = tokio::time::sleep(Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => (KillReason::Cancelled, None),
            _ = &mut deadline => (KillReason::TimedOut, None),
            status = &mut wait => (KillReason::Finished, status.ok().and_then(|status| status.code())),
        }
    };
    if !matches!(reason, KillReason::Finished) {
        terminate_child_tree(&mut child, child_pid).await;
    }
    let (stdout, stderr) = match timeout(Duration::from_secs(2), drain_handle).await {
        Ok(Ok(values)) => values,
        _ => (String::new(), String::new()),
    };
    let stderr_suffix = match reason {
        KillReason::Finished => None,
        KillReason::Cancelled => Some("[aborted]".to_string()),
        KillReason::TimedOut => Some(format!("[timed out after {timeout_secs}s]")),
    };
    Ok(RunOutcome {
        stdout,
        stderr,
        exit_code,
        stderr_suffix,
    })
}

enum KillReason {
    Finished,
    TimedOut,
    Cancelled,
}

async fn terminate_child_tree(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
    let _ = pid;
}

fn sensitive_environment_name(name: &OsStr) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    upper == "ANTHROPIC_API_KEY"
        || upper == "OPENAI_API_KEY"
        || upper.ends_with("_KEY")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET")
        || upper.ends_with("PASSWORD")
}

fn requested_timeout_secs(params: &Value) -> u64 {
    params
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS)
}

static DEFINITION: LazyLock<Tool> = LazyLock::new(|| Tool {
    name: "bash".into(),
    description: format!(
        "Run a shell command in the session workspace via sh -c. Returns tail-truncated stdout/stderr and the exit code. Timeout defaults to {DEFAULT_TIMEOUT_SECS}s and is capped at {MAX_TIMEOUT_SECS}s; timeout and cancellation kill the process group."
    ),
    parameters: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "command": { "type": "string", "description": "Shell command to execute." },
            "timeout": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS, "description": "Timeout in seconds (default 120, maximum 600)." }
        },
        "required": ["command"]
    }),
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_configured_secret_environment_names() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "github_token",
            "client_secret",
            "DATABASE_PASSWORD",
        ] {
            assert!(sensitive_environment_name(OsStr::new(name)), "{name}");
        }
        for name in ["HOME", "PATH", "SHELL", "DATABASE_URL"] {
            assert!(!sensitive_environment_name(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn timeout_defaults_and_clamps_to_the_documented_range() {
        assert_eq!(requested_timeout_secs(&json!({})), 120);
        assert_eq!(requested_timeout_secs(&json!({ "timeout": 0 })), 1);
        assert_eq!(requested_timeout_secs(&json!({ "timeout": 17 })), 17);
        assert_eq!(requested_timeout_secs(&json!({ "timeout": 601 })), 600);
    }
}
