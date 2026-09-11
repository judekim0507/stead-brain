use std::time::Duration;

use serde_json::{Value, json};
use steadwright_cdp::chromium::{self, LaunchOptions};
use steadwright_cdp::{CdpError, Connection, WebSocketTransport, auto_attach};

#[tokio::test]
async fn chromium_cdp_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
    let Some(executable) = chromium::find_executable() else {
        println!("skipping live tests: no Chromium found");
        return Ok(());
    };
    let chromium = chromium::launch(LaunchOptions {
        executable: Some(executable),
        ..LaunchOptions::default()
    })
    .await?;
    let transport = WebSocketTransport::connect(&chromium.ws_url).await?;
    let connection = Connection::new(transport);

    let version = connection
        .send("Browser.getVersion", json!({}), None)
        .await?;
    let product = version["product"].as_str().unwrap_or_default();
    assert!(
        product.starts_with("Chrome/") || product.starts_with("HeadlessChrome/"),
        "unexpected browser product: {product}"
    );

    let targets = connection
        .send("Target.getTargets", json!({}), None)
        .await?;
    assert!(
        targets["targetInfos"]
            .as_array()
            .is_some_and(|targets| targets.iter().any(|target| target["type"] == "page")),
        "Target.getTargets did not include a page: {targets}"
    );

    let mut target_events = connection.events();
    auto_attach(&connection, None).await?;
    let attached = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = target_events.recv().await?;
            // Stead bundles extensions whose background pages attach too;
            // wait for the ordinary about:blank tab.
            if event.method == "Target.attachedToTarget"
                && event.params["targetInfo"]["type"] == "page"
                && event.params["targetInfo"]["url"] == "about:blank"
            {
                return Ok::<_, tokio::sync::broadcast::error::RecvError>(event);
            }
        }
    })
    .await??;
    let session_id = attached.params["sessionId"]
        .as_str()
        .expect("attachedToTarget event must contain sessionId");
    let session = connection.session(session_id);

    session.send("Page.enable", json!({})).await?;
    let mut page_events = session.events();
    session
        .send(
            "Page.navigate",
            json!({"url": "data:text/html,<title>hi</title>"}),
        )
        .await?;
    // The initial about:blank load may still be in flight when Page.enable
    // lands, so the first load event is not necessarily ours: check the
    // title after each one until the data: document has loaded.
    let evaluated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = page_events.recv().await?;
            if event.method != "Page.loadEventFired" {
                continue;
            }
            let evaluated = session
                .send(
                    "Runtime.evaluate",
                    json!({"expression": "document.title", "returnByValue": true}),
                )
                .await?;
            if evaluated["result"]["value"] == Value::String("hi".into()) {
                return Ok::<_, CdpError>(evaluated);
            }
        }
    })
    .await??;
    assert_eq!(evaluated["result"]["value"], Value::String("hi".into()));

    let bad = connection
        .send("Target.attachToTarget", json!({"targetId": "nope"}), None)
        .await;
    assert!(matches!(bad, Err(CdpError::Protocol { .. })));

    connection.close().await;
    drop(chromium);
    Ok(())
}
