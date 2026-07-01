//! `read` tool. Modeled on `packages/coding-agent/src/core/tools/read.ts` — same name + same
//! parameter shape (`path`, optional `offset` 1-indexed, optional `limit`).
//!
//! Simpler than the TS version: text-only (no image attachments), no compact-resource
//! classification, no per-extension truncation. Plenty for a "simple" coding agent.

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

pub struct ReadTool;

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
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `path`"))?;
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_LINES);

        let raw = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| AgentToolError::from(format!("read {path}: {e}")))?;

        // 1-indexed line offset.
        let skip = offset.saturating_sub(1);
        let mut taken_lines: Vec<&str> = Vec::with_capacity(limit.min(1024));
        let mut total_lines = 0usize;
        for line in raw.split_inclusive('\n') {
            total_lines += 1;
            if total_lines <= skip {
                continue;
            }
            if taken_lines.len() >= limit {
                break;
            }
            taken_lines.push(line);
        }
        let slice: String = taken_lines.concat();
        let (slice, trunc) = truncate_head(&slice, limit, DEFAULT_MAX_BYTES);

        let mut text = format!("[{path}] lines {}-{}\n", skip + 1, skip + trunc.kept_lines);
        if let Some(note) = trunc.note() {
            text.push_str(&note);
            text.push('\n');
        }
        text.push_str(&slice);

        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({
                "path": path,
                "totalLines": total_lines,
                "keptLines": trunc.kept_lines,
                "offset": offset,
            }),
            terminate: None,
        })
    }
}

use once_cell::sync::Lazy;
static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "read".into(),
    description: format!(
        "Read the contents of a UTF-8 text file. Use offset/limit for large files; output is \
         truncated to {DEFAULT_MAX_LINES} lines or {} KiB (whichever first).",
        DEFAULT_MAX_BYTES / 1024
    ),
    parameters: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Max lines to read" },
        },
        "required": ["path"],
    }),
});
