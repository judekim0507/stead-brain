// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: resolve directories through the Stead session path policy.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::PathPolicy;
use super::truncate::DEFAULT_MAX_BYTES;

const DEFAULT_LIMIT: usize = 500;

pub struct LsTool {
    paths: Arc<PathPolicy>,
}

impl LsTool {
    pub fn new(paths: Arc<PathPolicy>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "ls"
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
        let raw_path = params.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = self.paths.resolve_existing(raw_path)?;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_LIMIT);
        let mut directory = tokio::fs::read_dir(&path)
            .await
            .map_err(|error| AgentToolError::from(format!("ls {raw_path}: {error}")))?;
        let mut entries = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|error| AgentToolError::from(format!("ls {raw_path}: {error}")))?
        {
            let metadata = entry
                .metadata()
                .await
                .map_err(|error| AgentToolError::from(format!("metadata: {error}")))?;
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                metadata.is_dir(),
                metadata.len(),
            ));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let total = entries.len();
        let mut text = format!("{raw_path} ({total} entries)\n");
        let mut shown = 0;
        for (name, directory, bytes) in entries.iter().take(limit) {
            let line = if *directory {
                format!("  {name}/\n")
            } else {
                format!("  {name} ({bytes} bytes)\n")
            };
            if text.len() + line.len() > DEFAULT_MAX_BYTES {
                break;
            }
            text.push_str(&line);
            shown += 1;
        }
        if shown < total {
            text.push_str(&format!("[truncated: showed {shown}/{total}]\n"));
        }
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({ "path": raw_path, "totalEntries": total, "shownEntries": shown }),
            terminate: None,
        })
    }
}

static DEFINITION: LazyLock<Tool> = LazyLock::new(|| Tool {
    name: "ls".into(),
    description: format!(
        "List one directory alphabetically, including dotfiles. Relative paths start in the session workspace. Directories end in /; output is limited to {DEFAULT_LIMIT} entries."
    ),
    parameters: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": { "type": "string", "description": "Directory to list (default: workspace)." },
            "limit": { "type": "integer", "minimum": 1, "description": "Maximum entries (default 500)." }
        }
    }),
});
