// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: use walkdir and enforce Stead session path policy for every candidate.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use super::PathPolicy;
use super::grep::simple_glob_matches;

const DEFAULT_LIMIT: usize = 200;

pub struct FindTool {
    paths: Arc<PathPolicy>,
}

impl FindTool {
    pub fn new(paths: Arc<PathPolicy>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl AgentTool for FindTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "find"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let glob = params
            .get("glob")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::from("missing `glob`"))?
            .to_string();
        let raw_path = params.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = self.paths.resolve_existing(raw_path)?;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_LIMIT);
        let paths = self.paths.clone();
        let cancel_clone = cancel.clone();
        let glob_for_worker = glob.clone();
        let (hits, stopped) = tokio::task::spawn_blocking(move || {
            let mut hits = Vec::new();
            let mut stopped = false;
            for entry in WalkDir::new(path)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                if cancel_clone.is_cancelled() {
                    break;
                }
                if !entry.file_type().is_file()
                    || !simple_glob_matches(&glob_for_worker, &entry.file_name().to_string_lossy())
                {
                    continue;
                }
                if hits.len() >= limit {
                    stopped = true;
                    break;
                }
                let display = entry.path().display().to_string();
                if paths.resolve_existing(&display).is_ok() {
                    hits.push(display);
                }
            }
            (hits, stopped)
        })
        .await
        .map_err(|error| AgentToolError::from(format!("find worker: {error}")))?;
        let mut text = if stopped {
            format!(
                "find {glob}: showing first {} hits (limit reached)\n",
                hits.len()
            )
        } else {
            format!("find {glob}: {} hits\n", hits.len())
        };
        for hit in &hits {
            text.push_str(hit);
            text.push('\n');
        }
        if stopped {
            text.push_str("... results truncated; use a narrower glob/path or a higher limit\n");
        }
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({ "paths": hits, "limit": limit, "stopped_at_limit": stopped }),
            terminate: None,
        })
    }
}

static DEFINITION: LazyLock<Tool> = LazyLock::new(|| Tool {
    name: "find".into(),
    description: format!(
        "Find files recursively by filename glob. Relative paths start in the session workspace. Output is limited to {DEFAULT_LIMIT} paths by default."
    ),
    parameters: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "glob": { "type": "string", "description": "Filename glob such as *.rs or README*." },
            "path": { "type": "string", "description": "Directory to search (default: workspace)." },
            "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of paths." }
        },
        "required": ["glob"]
    }),
});
