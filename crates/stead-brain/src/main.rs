use anyhow::Context;
use stead_brain_core::{BrainCore, BrainError, make_error};
use stead_brain_protocol::{
    BrainEvent, BrowserRequest, PROTOCOL_VERSION, ResponseEnvelope, encode_response_line,
    parse_request_line,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ResponseEnvelope>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());
        while let Some(response) = out_rx.recv().await {
            write_response(&mut stdout, response).await?;
            stdout.flush().await?;
        }
        anyhow::Ok(())
    });
    let mut core: Option<BrainCore> = None;

    while let Some(line) = lines.next_line().await.context("read stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = match parse_request_line(&line) {
            Ok(request) => request,
            Err(error) => {
                send_response(
                    &out_tx,
                    make_error(None, "malformed_json", error.to_string()),
                );
                continue;
            }
        };

        let request_id = parsed.request_id.clone();
        if parsed.protocol_version != PROTOCOL_VERSION {
            send_response(
                &out_tx,
                make_error(
                    Some(request_id),
                    "protocol_version_mismatch",
                    format!(
                        "expected protocol_version {}, got {}",
                        PROTOCOL_VERSION, parsed.protocol_version
                    ),
                ),
            );
            continue;
        }

        let shutdown = matches!(parsed.message, BrowserRequest::Shutdown);
        if let BrowserRequest::SendMessage(params) = parsed.message {
            match initialized(&core) {
                Ok(core_ref) => {
                    let core = core_ref.clone();
                    let request_id_for_error = request_id.clone();
                    let tx = out_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = core
                            .send_message_stream(request_id_for_error.clone(), params, tx.clone())
                            .await
                        {
                            send_response(
                                &tx,
                                make_error(
                                    Some(request_id_for_error),
                                    error_code(&error),
                                    error.to_string(),
                                ),
                            );
                        }
                    });
                }
                Err(error) => {
                    send_response(
                        &out_tx,
                        make_error(Some(request_id), error_code(&error), error.to_string()),
                    );
                }
            }
            continue;
        }

        if let BrowserRequest::StartProviderOAuth(params) = parsed.message {
            match initialized(&core) {
                Ok(core_ref) => {
                    let core = core_ref.clone();
                    let request_id_for_error = request_id.clone();
                    let tx = out_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = core
                            .start_provider_oauth(request_id_for_error.clone(), params, tx.clone())
                            .await
                        {
                            send_response(
                                &tx,
                                make_error(
                                    Some(request_id_for_error),
                                    error_code(&error),
                                    error.to_string(),
                                ),
                            );
                        }
                    });
                }
                Err(error) => {
                    send_response(
                        &out_tx,
                        make_error(Some(request_id), error_code(&error), error.to_string()),
                    );
                }
            }
            continue;
        }

        let responses = handle_request(&mut core, parsed.message, request_id).await;
        match responses {
            Ok(responses) => {
                for response in responses {
                    send_response(&out_tx, response);
                }
            }
            Err(error) => {
                send_response(
                    &out_tx,
                    make_error(None, error_code(&error), error.to_string()),
                );
            }
        }
        if shutdown {
            break;
        }
    }

    drop(out_tx);
    writer.await??;
    Ok(())
}

async fn handle_request(
    core: &mut Option<BrainCore>,
    message: BrowserRequest,
    request_id: String,
) -> stead_brain_core::Result<Vec<ResponseEnvelope>> {
    match message {
        BrowserRequest::Initialize(params) => {
            let (initialized, ready) = BrainCore::initialize(params).await?;
            *core = Some(initialized);
            Ok(vec![ResponseEnvelope::event(
                Some(request_id),
                BrainEvent::Ready(ready),
            )])
        }
        BrowserRequest::Shutdown => Ok(Vec::new()),
        BrowserRequest::CreateSession(params) => {
            initialized(core)?.create_session(request_id, params).await
        }
        BrowserRequest::ListSessions => initialized(core)?.list_sessions(request_id).await,
        BrowserRequest::LoadSession { session_id } => {
            initialized(core)?
                .load_session(request_id, session_id)
                .await
        }
        BrowserRequest::SendMessage(params) => {
            initialized(core)?.send_message(request_id, params).await
        }
        BrowserRequest::CancelTurn { session_id } => {
            initialized(core)?.cancel_turn(request_id, session_id).await
        }
        BrowserRequest::ToolResult(result) => {
            initialized(core)?
                .accept_tool_result(request_id, result)
                .await
        }
        BrowserRequest::ListModels => initialized(core)?.list_models(request_id).await,
        BrowserRequest::ListProviderAuth => initialized(core)?.list_provider_auth(request_id).await,
        BrowserRequest::SetProviderCredential(params) => {
            initialized(core)?
                .set_provider_credential(request_id, params)
                .await
        }
        BrowserRequest::StartProviderOAuth(params) => {
            initialized(core)?
                .start_provider_oauth(request_id, params, mpsc::unbounded_channel().0)
                .await?;
            Ok(Vec::new())
        }
        BrowserRequest::ImportCodexAuth(params) => {
            initialized(core)?
                .import_codex_auth(request_id, params)
                .await
        }
        BrowserRequest::ClearProviderCredential { provider } => {
            initialized(core)?
                .clear_provider_credential(request_id, provider)
                .await
        }
    }
}

fn initialized(core: &Option<BrainCore>) -> stead_brain_core::Result<&BrainCore> {
    core.as_ref().ok_or(BrainError::Uninitialized)
}

async fn write_response(
    stdout: &mut tokio::io::BufWriter<tokio::io::Stdout>,
    response: ResponseEnvelope,
) -> anyhow::Result<()> {
    let encoded = encode_response_line(&response).context("encode response")?;
    stdout
        .write_all(encoded.as_bytes())
        .await
        .context("write stdout")
}

fn send_response(tx: &mpsc::UnboundedSender<ResponseEnvelope>, response: ResponseEnvelope) {
    let _ = tx.send(response);
}

fn error_code(error: &BrainError) -> &'static str {
    match error {
        BrainError::Uninitialized => "uninitialized",
        BrainError::SessionNotFound(_) => "session_not_found",
        BrainError::InvalidRequest(_) => "invalid_request",
        BrainError::FileAccessDenied(_) => "file_access_denied",
        BrainError::ModelNotConfigured => "model_not_configured",
        BrainError::ModelNotFound { .. } => "model_not_found",
        BrainError::AgentRun(_) => "agent_run_failed",
        BrainError::ProviderAuth(_) => "provider_auth_failed",
        BrainError::Io(_) => "io_error",
        BrainError::Json(_) => "json_error",
    }
}
