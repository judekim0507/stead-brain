//! Stand-in for the stead-brain helper: proves the browser's fd 3/4 CDP
//! bridge end to end. Speaks just enough of the stdio protocol to be
//! launched, then runs two CDP commands over the fd pair and logs them.
use serde_json::json;
use std::io::Write;
use steadwright_cdp::{Connection, FdPairTransport};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    let mut log = std::fs::File::create("/tmp/fd_probe.log").unwrap();
    let transport = match FdPairTransport::from_raw_fds(3, 4) {
        Ok(t) => t,
        Err(e) => {
            writeln!(log, "transport error: {e}").unwrap();
            return;
        }
    };
    let conn = Connection::new(transport);
    match conn.send("Browser.getVersion", json!({}), None).await {
        Ok(v) => writeln!(log, "getVersion ok: {}", v["product"]).unwrap(),
        Err(e) => writeln!(log, "getVersion err: {e}").unwrap(),
    }
    match conn.send("Target.getTargets", json!({}), None).await {
        Ok(v) => writeln!(
            log,
            "targets: {}",
            v["targetInfos"].as_array().map(|a| a.len()).unwrap_or(0)
        )
        .unwrap(),
        Err(e) => writeln!(log, "getTargets err: {e}").unwrap(),
    }
    match conn.send("Stead.listTabs", json!({}), None).await {
        Ok(v) => writeln!(log, "listTabs ok: {v}").unwrap(),
        Err(e) => writeln!(log, "listTabs err: {e}").unwrap(),
    }
    if let Ok(v) = conn.send("Target.getTargets", json!({}), None).await {
        if let Some(t) = v["targetInfos"]
            .as_array()
            .and_then(|a| a.iter().find(|t| t["type"] == "page"))
        {
            let id = t["targetId"].clone();
            match conn
                .send("Stead.describeTarget", json!({"targetId": id}), None)
                .await
            {
                Ok(v) => writeln!(log, "describeTarget ok: {v}").unwrap(),
                Err(e) => writeln!(log, "describeTarget err: {e}").unwrap(),
            }
        }
    }
    match conn.send("Stead.nope", json!({}), None).await {
        Ok(v) => writeln!(log, "unknown ok?: {v}").unwrap(),
        Err(e) => writeln!(log, "unknown err (expected): {e}").unwrap(),
    }
    log.flush().unwrap();
    // Keep the stdio protocol alive so the browser does not restart us.
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();
    while let Ok(Some(line)) = lines.next_line().await {
        let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        let id = req["request_id"].clone();
        let resp =
            json!({"type":"ready","request_id":id,"protocol_version":1,"skills":[],"tools":[]});
        let _ = stdout.write_all(format!("{resp}\n").as_bytes()).await;
        let _ = stdout.flush().await;
    }
}
