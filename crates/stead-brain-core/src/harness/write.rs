// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: resolve and classify writes through the Stead session policy.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::PathPolicy;

pub struct WriteTool {
    paths: Arc<PathPolicy>,
}

impl WriteTool {
    pub fn new(paths: Arc<PathPolicy>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "write"
    }

    fn permission_classification(&self, params: &Value) -> PermissionClassification {
        let Some(path) = params.get("path").and_then(Value::as_str) else {
            return PermissionClassification::Allow;
        };
        match self.paths.classify_write(path) {
            Some(reason) => PermissionClassification::Block { reason },
            None => PermissionClassification::Allow,
        }
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let raw_path = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::from("missing `path`"))?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::from("missing `content`"))?;
        let path = self.paths.resolve_write(raw_path)?;
        let parent = path
            .parent()
            .ok_or_else(|| AgentToolError::from("path has no parent"))?;
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            AgentToolError::from(format!("create {}: {error}", parent.display()))
        })?;
        let path = self.paths.resolve_write(raw_path)?;
        tokio::fs::write(&path, content.as_bytes())
            .await
            .map_err(|error| AgentToolError::from(format!("write {raw_path}: {error}")))?;
        let bytes = content.len();
        let lines = content.lines().count();
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "Wrote {bytes} bytes ({lines} lines) to {raw_path}"
            ))],
            details: json!({ "path": raw_path, "bytes": bytes, "lines": lines }),
            terminate: None,
        })
    }
}

static DEFINITION: LazyLock<Tool> = LazyLock::new(|| {
    Tool {
    name: "write".into(),
    description: "Write or overwrite a UTF-8 text file. Relative paths start in the session workspace; parent directories are created. Put user-facing deliverables under artifacts/.".into(),
    parameters: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": { "type": "string", "description": "Workspace-relative or permitted absolute file path." },
            "content": { "type": "string", "description": "Complete UTF-8 file contents." }
        },
        "required": ["path", "content"]
    }),
}
});
