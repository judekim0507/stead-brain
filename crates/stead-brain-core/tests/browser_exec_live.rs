use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use stead_brain_core::{BrainError, BrowserToolBridge, browser_tools};
use stead_brain_protocol::ToolResultPayload;
use steadwright_cdp::chromium::{self, LaunchOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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
        // The network smoke test intentionally fetches a loopback fixture from
        // an opaque data: origin. Production pages do not need this test-only
        // relaxation because they have a normal origin.
        extra_args: vec!["--disable-web-security".into()],
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

    let (api_url, api_server) = start_api_server().await?;

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

    let api_url_js = serde_json::to_string(&api_url)?;
    let data_url = "data:text/html,<title>Network fixture</title>";
    let data_url_js = serde_json::to_string(&data_url)?;
    let network = tool
        .execute(
            "live-network",
            json!({"code": format!(r#"
                await page.clearRequests();
                await page.goto({data_url_js});
                const responsePromise = page.waitForResponse({api_url_js});
                await page.evaluate(url => fetch(url).then(response => response.text()), {api_url_js});
                const response = await responsePromise;
                const parsed = await response.json();
                const text = await response.text();
                const bytes = await response.body();
                return {{
                    response: {{
                        status: response.status(),
                        parsed,
                        text,
                        requestMethod: response.request().method(),
                        bodyIsUint8Array: bytes instanceof Uint8Array,
                    }},
                    requests: await page.requests({{url: '**/api/**', withBodies: true}}),
                }};
            "#)}),
            CancellationToken::new(),
            None,
        )
        .await?;
    let network: Value = serde_json::from_str(result_text(&network))?;
    assert_eq!(network["response"]["status"], 200);
    assert_eq!(network["response"]["parsed"], json!({"items": [1, 2]}));
    assert_eq!(network["response"]["requestMethod"], "GET");
    assert_eq!(network["response"]["bodyIsUint8Array"], true);
    assert!(
        network["response"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("items"))
    );
    let requests = network["requests"]
        .as_array()
        .expect("page.requests must return an array");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "GET");
    assert_eq!(requests[0]["status"], 200);
    assert_eq!(requests[0]["resourceType"], "fetch");
    assert_eq!(requests[0]["ok"], true);
    assert!(
        requests[0]["bodyPreview"]
            .as_str()
            .is_some_and(|preview| preview.contains("\n  \"items\""))
    );

    api_server.await??;

    drop(chromium);
    Ok(())
}

async fn start_api_server()
-> std::io::Result<(String, tokio::task::JoinHandle<std::io::Result<()>>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let url = format!("http://{}/api/items", listener.local_addr()?);
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::with_capacity(1024);
            let mut chunk = [0_u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let preflight = request.starts_with(b"OPTIONS ");
            let body: &[u8] = if preflight {
                b""
            } else {
                br#"{"items":[1,2]}"#
            };
            let status = if preflight {
                "204 No Content"
            } else {
                "200 OK"
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nAccess-Control-Allow-Private-Network: true\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.shutdown().await?;
            if !preflight {
                break;
            }
        }
        Ok(())
    });
    Ok((url, server))
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
