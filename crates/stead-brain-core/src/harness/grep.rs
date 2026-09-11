// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: use walkdir and enforce Stead session path policy for every candidate.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use super::PathPolicy;

const DEFAULT_MAX_MATCHES: usize = 200;
const DEFAULT_MAX_FILES: usize = 5_000;
const MAX_MATCH_LINE_CHARS: usize = 500;

pub struct GrepTool {
    paths: Arc<PathPolicy>,
}

impl GrepTool {
    pub fn new(paths: Arc<PathPolicy>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "grep"
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
        let pattern = params
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentToolError::from("missing `pattern`"))?;
        let raw_path = params.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = self.paths.resolve_existing(raw_path)?;
        let glob = params
            .get("glob")
            .and_then(Value::as_str)
            .map(str::to_string);
        let case_insensitive = params
            .get("case_insensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_MAX_MATCHES);
        let mut builder = regex::RegexBuilder::new(pattern);
        builder.case_insensitive(case_insensitive);
        let regex = builder
            .build()
            .map_err(|error| AgentToolError::from(format!("regex: {error}")))?;
        let paths = self.paths.clone();
        let cancel_clone = cancel.clone();
        let matches = tokio::task::spawn_blocking(move || {
            search_tree(&path, glob.as_deref(), &regex, limit, &paths, &cancel_clone)
        })
        .await
        .map_err(|error| AgentToolError::from(format!("grep worker: {error}")))?;
        let truncated_lines = matches.iter().filter(|value| value.truncated).count();
        let mut text = format!("grep: {} hits\n", matches.len());
        for value in &matches {
            text.push_str(&format!("{}:{}: {}\n", value.path, value.line, value.text));
        }
        if truncated_lines > 0 {
            text.push_str(&format!(
                "[{truncated_lines} long matching line(s) truncated to {MAX_MATCH_LINE_CHARS} chars]\n"
            ));
        }
        if matches.len() >= limit {
            text.push_str(&format!("[truncated at {limit} matches]\n"));
        }
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({ "matches": matches.len(), "truncated_lines": truncated_lines, "max_match_line_chars": MAX_MATCH_LINE_CHARS }),
            terminate: None,
        })
    }
}

struct MatchOut {
    path: String,
    line: usize,
    text: String,
    truncated: bool,
}

fn search_tree(
    root: &std::path::Path,
    glob: Option<&str>,
    regex: &Regex,
    limit: usize,
    paths: &PathPolicy,
    cancel: &CancellationToken,
) -> Vec<MatchOut> {
    let mut output = Vec::new();
    let mut files = 0;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if cancel.is_cancelled() || files >= DEFAULT_MAX_FILES || output.len() >= limit {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if let Some(glob) = glob {
            let name = entry.file_name().to_string_lossy();
            if !simple_glob_matches(glob, &name) {
                continue;
            }
        }
        let display = entry.path().display().to_string();
        let Ok(path) = paths.resolve_existing(&display) else {
            continue;
        };
        files += 1;
        let Ok(body) = std::fs::read_to_string(path) else {
            continue;
        };
        for (line, value) in body.lines().enumerate() {
            let Some(found) = regex.find(value) else {
                continue;
            };
            let (text, truncated) = preview_match_line(value, found);
            output.push(MatchOut {
                path: display.clone(),
                line: line + 1,
                text,
                truncated,
            });
            if output.len() >= limit {
                break;
            }
        }
    }
    output
}

pub(super) fn simple_glob_matches(pattern: &str, name: &str) -> bool {
    let mut regex = String::from("^");
    for character in pattern.chars() {
        match character {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            _ => regex.push_str(&regex::escape(&character.to_string())),
        }
    }
    regex.push('$');
    Regex::new(&regex).is_ok_and(|regex| regex.is_match(name))
}

fn preview_match_line(line: &str, found: regex::Match<'_>) -> (String, bool) {
    if line.chars().count() <= MAX_MATCH_LINE_CHARS {
        return (line.to_string(), false);
    }
    let match_start = line[..found.start()].chars().count();
    let match_len = line[found.start()..found.end()].chars().count().max(1);
    let visible_match = match_len.min(MAX_MATCH_LINE_CHARS);
    let context = MAX_MATCH_LINE_CHARS - visible_match;
    let start = match_start.saturating_sub(context / 2);
    let end = (match_start + visible_match + context - context / 2).min(line.chars().count());
    let mut preview = String::new();
    if start > 0 {
        preview.push_str("[line truncated]...");
    }
    preview.extend(line.chars().skip(start).take(end - start));
    if end < line.chars().count() {
        preview.push_str("...[line truncated]");
    }
    (preview, true)
}

static DEFINITION: LazyLock<Tool> = LazyLock::new(|| Tool {
    name: "grep".into(),
    description: format!(
        "Search UTF-8 files for lines matching a regex. Relative paths start in the session workspace. Optional glob filters filenames; output is limited to {DEFAULT_MAX_MATCHES} matches."
    ),
    parameters: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "pattern": { "type": "string", "description": "Regular expression to search for." },
            "path": { "type": "string", "description": "Directory or file to search (default: workspace)." },
            "glob": { "type": "string", "description": "Optional filename glob such as *.rs." },
            "case_insensitive": { "type": "boolean", "description": "Use case-insensitive matching." },
            "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of matches." }
        },
        "required": ["pattern"]
    }),
});
