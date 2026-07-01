//! `task` tool — subagent / Task delegation. Spawns a fresh AgentHarness with an in-memory
//! session, runs a sub-prompt to completion (its own loop, its own iteration budget), and
//! returns the final assistant text to the parent agent as a single tool result.
//!
//! v1 scope:
//! - One subagent spec, "general": same model as parent, read-only tools (read/grep/find/ls/web_fetch),
//!   max 16 iterations, MemorySessionStorage so nothing leaks to disk.
//! - Concurrent execution mode (Parallel) so the parent can fire multiple Task calls in one
//!   turn and they run together.
//! - Parent abort cascades: the tool listens on the parent's cancellation token and aborts
//!   its inner harness immediately.
//!
//! Out of scope (follow-ups under #11):
//! - User-defined subagent specs via `~/.pie/subagents/*.toml`.
//! - Recursive sub-subagents (we'd need a depth cap).
//! - Cost rollup into the parent CostTracker (each subagent has its own tracker for now).

use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use pie_agent_core::{
    AgentEvent, AgentHarness, AgentHarnessOptions, AgentMessage, AgentTool, AgentToolError,
    AgentToolResult, AgentToolUpdate, MemorySessionStorage, Session, SessionStorage, StreamFn,
    ToolExecutionMode,
};
use pie_ai::{Message as PiMessage, Model, Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SUBAGENT_TYPES: &[&str] = &["general"];

/// Closure that builds the tool set a subagent should have access to. Built at parent-harness
/// construction so each subagent starts with the same set of (read-only) capabilities.
pub type SubagentToolsFn = Arc<dyn Fn() -> Vec<Arc<dyn AgentTool>> + Send + Sync>;

pub struct TaskTool {
    /// Model used by spawned subagents. Cloned from the parent at construction time so a
    /// later `/model` switch doesn't change in-flight subagent settings.
    model: Model,
    /// Optional stream_fn shared with the parent. `None` falls back to `pie_ai::stream_simple`.
    stream_fn: Option<StreamFn>,
    /// Factory for the subagent tool set. Read-only by convention so subagents can't write.
    subagent_tools: SubagentToolsFn,
}

impl TaskTool {
    pub fn new(model: Model, stream_fn: Option<StreamFn>, subagent_tools: SubagentToolsFn) -> Self {
        Self {
            model,
            stream_fn,
            subagent_tools,
        }
    }
}

#[async_trait]
impl AgentTool for TaskTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }
    fn label(&self) -> &str {
        "task"
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        parent_cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let subagent_type = params
            .get("subagent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("general");
        if !SUBAGENT_TYPES.contains(&subagent_type) {
            return Err(AgentToolError::Message(format!(
                "unknown subagent_type: {subagent_type} (allowed: {})",
                SUBAGENT_TYPES.join(", ")
            )));
        }
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::Message("missing required arg: prompt".into()))?
            .to_string();
        let description = params
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Fresh in-memory session for the subagent. Nothing touches disk.
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage as Arc<dyn SessionStorage>);
        let mut opts = AgentHarnessOptions::new(self.model.clone(), session.clone());
        opts.system_prompt = format!(
            "You are a research subagent dispatched by a coding agent.\n\
             Description of your task: {description}\n\
             Stay focused on the prompt; return a concise final answer."
        );
        opts.tools = (self.subagent_tools)();
        opts.stream_fn = self.stream_fn.clone();
        let sub = Arc::new(AgentHarness::new(opts));

        // Collect the final assistant text. The Agent emits AgentEvent::MessageEnd for every
        // assistant turn; subscribe and keep the latest text seen.
        let final_text: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let collector = final_text.clone();
        let _unsub = sub.agent().subscribe(Arc::new(move |event, _| {
            let collector = collector.clone();
            Box::pin(async move {
                if let AgentEvent::MessageEnd {
                    message: AgentMessage::Llm(PiMessage::Assistant(a)),
                } = event
                {
                    let text = a
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            pie_ai::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        *collector.lock() = text;
                    }
                }
            })
        }));

        // Parent abort cascades to the subagent. Spawn a tiny watcher that flips the inner
        // cancel when the outer one does.
        let sub_for_cancel = sub.clone();
        let parent_cancel_clone = parent_cancel.clone();
        let watcher = tokio::spawn(async move {
            parent_cancel_clone.cancelled().await;
            sub_for_cancel.abort();
        });

        let run = sub.prompt(prompt).await;
        watcher.abort();

        if parent_cancel.is_cancelled() {
            return Err(AgentToolError::Message("cancelled".into()));
        }
        if let Err(e) = run {
            return Err(AgentToolError::Message(format!("subagent failed: {e}")));
        }
        let result = std::mem::take(&mut *final_text.lock());
        let body = if result.is_empty() {
            "(subagent produced no text output)".to_string()
        } else {
            result
        };
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(body.clone())],
            details: json!({
                "subagent_type": subagent_type,
                "description": description,
                "chars": body.len(),
            }),
            terminate: None,
        })
    }
}

static DEFINITION: Lazy<Tool> = Lazy::new(|| {
    Tool {
    name: "task".into(),
    description:
        "Delegate a self-contained research task to a fresh sub-agent. The subagent gets its own context window and tool set; this tool returns a single text result from the subagent. Use this when you need to inspect a large surface area (search, file reads) without polluting the main conversation.".into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "subagent_type": {
                "type": "string",
                "enum": SUBAGENT_TYPES,
                "description": "Which subagent kind to spawn. v1 ships only 'general'.",
                "default": "general",
            },
            "description": {
                "type": "string",
                "description": "Short label for the task (visible in UI logs).",
            },
            "prompt": {
                "type": "string",
                "description": "Full prompt the subagent will receive as its user message.",
            },
        },
        "required": ["prompt"],
        "additionalProperties": false,
    }),
}
});
