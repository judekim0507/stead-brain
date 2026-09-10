// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: resolve paths through the Stead session policy.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::PathPolicy;
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

pub struct ReadTool {
    paths: Arc<PathPolicy>,
}

impl ReadTool {
    pub fn new(paths: Arc<PathPolicy>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "read"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
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
        let path = self.paths.resolve_existing(raw_path)?;
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(1) as usize;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_MAX_LINES);
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| AgentToolError::from(format!("read {raw_path}: {error}")))?;
        let skip = offset.saturating_sub(1);
        let lines = body
            .split_inclusive('\n')
            .skip(skip)
            .take(limit)
            .collect::<String>();
        let total_lines = body.split_inclusive('\n').count();
        let (slice, truncation) = truncate_head(&lines, limit, DEFAULT_MAX_BYTES);
        let mut text = format!(
            "[{raw_path}] lines {}-{}\n",
            skip + 1,
            skip + truncation.kept_lines
        );
        if let Some(note) = truncation.note() {
            text.push_str(&note);
            text.push('\n');
        }
        text.push_str(&slice);
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({ "path": raw_path, "totalLines": total_lines, "keptLines": truncation.kept_lines, "offset": offset }),
            terminate: None,
        })
    }
}

static DEFINITION: LazyLock<Tool> = LazyLock::new(|| Tool {
    name: "read".into(),
    description: format!(
        "Read a UTF-8 text file. Relative paths start in the session workspace. Use offset/limit for large files; output is capped at {DEFAULT_MAX_LINES} lines or {} KiB.",
        DEFAULT_MAX_BYTES / 1024
    ),
    parameters: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": { "type": "string", "description": "Workspace-relative or permitted absolute file path." },
            "offset": { "type": "integer", "minimum": 1, "description": "First line to read (1-indexed)." },
            "limit": { "type": "integer", "minimum": 1, "description": "Maximum lines to read." }
        },
        "required": ["path"]
    }),
});
