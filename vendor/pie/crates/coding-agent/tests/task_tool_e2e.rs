//! End-to-end test for the subagent / Task tool (issue #11).
//!
//! Drives `TaskTool::execute` with a faux StreamFn shared with the inner subagent harness.
//! Verifies:
//!   1. The tool returns the subagent's final assistant text.
//!   2. Unknown subagent_type errors clearly.
//!   3. Missing required `prompt` arg errors clearly.

use std::sync::Arc;

use pie_agent_core::{AgentTool, StreamFn};
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, AssistantRole,
    ContentBlock, DoneReason, ModelCost, StopReason, Usage,
};
use tokio_util::sync::CancellationToken;

#[path = "../src/tools/task.rs"]
mod task;

fn faux_model() -> pie_ai::Model {
    pie_ai::Model {
        id: "faux".into(),
        name: "Faux".into(),
        api: pie_ai::Api::from("faux"),
        provider: pie_ai::Provider::from("faux"),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
    }
}

fn faux_stream(text: &'static str) -> StreamFn {
    Arc::new(move |_, _, _| {
        let (stream, mut sender) = AssistantMessageEventStream::new();
        tokio::spawn(async move {
            let msg = AssistantMessage {
                role: AssistantRole::Assistant,
                content: vec![ContentBlock::text(text)],
                api: pie_ai::Api::from("faux"),
                provider: pie_ai::Provider::from("faux"),
                model: "faux".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };
            sender.push(AssistantMessageEvent::Start {
                partial: msg.clone(),
            });
            sender.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: msg,
            });
        });
        stream
    })
}

#[tokio::test]
async fn task_returns_subagent_final_text() {
    let tool = task::TaskTool::new(
        faux_model(),
        Some(faux_stream("subagent result")),
        Arc::new(Vec::new),
    );
    let res = tool
        .execute(
            "t-1",
            serde_json::json!({
                "subagent_type": "general",
                "description": "look up X",
                "prompt": "tell me about X",
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let body = match &res.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text content"),
    };
    assert_eq!(body, "subagent result");
}

#[tokio::test]
async fn task_unknown_subagent_type_errors() {
    let tool = task::TaskTool::new(faux_model(), Some(faux_stream("nope")), Arc::new(Vec::new));
    let err = tool
        .execute(
            "t-2",
            serde_json::json!({
                "subagent_type": "nope",
                "prompt": "x",
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown subagent_type"), "{err}");
}

#[tokio::test]
async fn task_missing_prompt_errors() {
    let tool = task::TaskTool::new(faux_model(), Some(faux_stream("nope")), Arc::new(Vec::new));
    let err = tool
        .execute("t-3", serde_json::json!({}), CancellationToken::new(), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing required arg: prompt"), "{err}");
}

#[tokio::test]
async fn task_parent_abort_cascades_to_subagent() {
    // Stalled subagent stream: subagent never finishes on its own; only parent abort can
    // unblock it.
    let stalled: StreamFn = Arc::new(move |_, _, _| {
        let (stream, sender) = AssistantMessageEventStream::new();
        tokio::spawn(async move {
            let _sender = sender;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        stream
    });
    let tool = task::TaskTool::new(faux_model(), Some(stalled), Arc::new(Vec::new));
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let exec = tokio::spawn(async move {
        tool.execute("t-4", serde_json::json!({ "prompt": "x" }), cancel2, None)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), exec)
        .await
        .expect("parent abort must unblock subagent within 2s")
        .expect("task panicked");
    let err = result.unwrap_err().to_string();
    assert!(
        err.to_lowercase().contains("cancel") || err.to_lowercase().contains("abort"),
        "expected abort error: {err}"
    );
}
