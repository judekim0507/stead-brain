// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: resolve and classify edits through the Stead session policy.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::PathPolicy;

pub struct EditTool {
    paths: Arc<PathPolicy>,
}

impl EditTool {
    pub fn new(paths: Arc<PathPolicy>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl AgentTool for EditTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "edit"
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
        let old = params
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::from("missing `old_string`"))?;
        let new = params
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::from("missing `new_string`"))?;
        let replace_all = params
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old == new {
            return Err(AgentToolError::from(
                "old_string must differ from new_string",
            ));
        }
        let path = self.paths.resolve_existing(raw_path)?;
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| AgentToolError::from(format!("read {raw_path}: {error}")))?;
        let occurrences = body.matches(old).count();
        if occurrences == 0 {
            return Err(AgentToolError::from(format!(
                "old_string not found in {raw_path}"
            )));
        }
        if occurrences > 1 && !replace_all {
            return Err(AgentToolError::from(format!(
                "old_string matched {occurrences} times in {raw_path}; use replace_all=true or include more context"
            )));
        }
        let replacement = if replace_all {
            body.replace(old, new)
        } else {
            body.replacen(old, new, 1)
        };
        let write_path = self.paths.resolve_write(raw_path)?;
        tokio::fs::write(&write_path, replacement.as_bytes())
            .await
            .map_err(|error| AgentToolError::from(format!("write {raw_path}: {error}")))?;
        let preview = render_diff_preview(old, new);
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "Edited {raw_path} ({occurrences} replacement{}).\n{preview}",
                if occurrences == 1 { "" } else { "s" }
            ))],
            details: json!({ "path": raw_path, "replacements": occurrences, "replaceAll": replace_all }),
            terminate: None,
        })
    }
}

fn render_diff_preview(old: &str, new: &str) -> String {
    let mut output = String::from("--- before\n");
    for line in old.lines().take(10) {
        output.push_str("- ");
        output.push_str(line);
        output.push('\n');
    }
    output.push_str("+++ after\n");
    for line in new.lines().take(10) {
        output.push_str("+ ");
        output.push_str(line);
        output.push('\n');
    }
    output
}

static DEFINITION: LazyLock<Tool> = LazyLock::new(|| {
    Tool {
    name: "edit".into(),
    description: "Replace an exact substring in a UTF-8 file. The old text must be unique unless replace_all is true. Relative paths start in the session workspace.".into(),
    parameters: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": { "type": "string", "description": "Workspace-relative or permitted absolute file path." },
            "old_string": { "type": "string", "description": "Exact text to replace; include context to make it unique." },
            "new_string": { "type": "string", "description": "Replacement text; use an empty string to delete." },
            "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring uniqueness." }
        },
        "required": ["path", "old_string", "new_string"]
    }),
}
});
