use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use stead_brain_core::{BrainError, BrowserToolBridge, browser_tools};
use stead_brain_protocol::ToolResultPayload;
use steadwright_cdp::chromium::{self, LaunchOptions};
use tokio_util::sync::CancellationToken;

struct UnusedBridge;

#[async_trait]
impl BrowserToolBridge for UnusedBridge {
    async fn call_browser_tool(
        &self,
        _tool_call_id: &str,
        _name: &str,
        _arguments: Value,
        _cancel: CancellationToken,
    ) -> Result<ToolResultPayload, BrainError> {
        panic!("the live smoke test does not use credentials")
    }
}

#[tokio::test]
async fn browser_exec_runs_against_chromium_and_persists_state()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(executable) = chromium::find_executable() else {
        println!("skipping browser_exec live test: no Chromium found");
        return Ok(());
    };
    let chromium = chromium::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await?;
    // SAFETY: this integration-test process owns its environment and performs
    // no concurrent environment reads before constructing the browser tool.
    unsafe { std::env::set_var("STEADWRIGHT_WS_URL", &chromium.ws_url) };

    let tool = browser_tools(Arc::new(UnusedBridge))
        .into_iter()
        .next()
        .expect("browser_exec must be registered");

    let title = tool
        .execute(
            "live-title",
            json!({"code": "await page.goto('data:text/html,<title>Steadwright</title><button>Continue</button>'); await page.screenshot(); await page.screenshot(); return await page.title();"}),
            CancellationToken::new(),
            None,
        )
        .await?;
    assert_eq!(result_text(&title), "Steadwright");
    assert_eq!(
        title
            .content
            .iter()
            .filter(|block| matches!(block, pie_ai::UserContentBlock::Image(image) if image.mime_type == "image/png"))
            .count(),
        1,
        "identical screenshots should attach once"
    );

    let snapshot = tool
        .execute(
            "live-aria",
            json!({"code": "globalThis.shouldNotPersist = true; state.n = (state.n || 0) + 1; const button = page.getByRole('button', {name: /Continue/}).first(); if (await button.count() !== 1) throw new Error('locator proxy failed'); return await page.ariaSnapshot();"}),
            CancellationToken::new(),
            None,
        )
        .await?;
    assert!(
        result_text(&snapshot).contains("[ref="),
        "aria snapshot did not contain a Playwright ref: {:?}",
        result_text(&snapshot)
    );

    let state = tool
        .execute(
            "live-state",
            json!({"code": "state.n = (state.n || 0) + 1; console.log('loaded'); console.warn('retrying'); return {n: state.n, leaked: typeof globalThis.shouldNotPersist};"}),
            CancellationToken::new(),
            None,
        )
        .await?;
    assert_eq!(
        result_text(&state),
        "{\n \"n\": 2,\n \"leaked\": \"undefined\"\n}\n--- console ---\nloaded\nwarn: retrying"
    );

    drop(chromium);
    Ok(())
}

fn result_text(result: &pie_agent_core::AgentToolResult) -> &str {
    result
        .content
        .iter()
        .find_map(|block| match block {
            pie_ai::UserContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .expect("browser_exec result must contain text")
}
