use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use crate::error::{CdpError, TransportError};
use crate::transport::Transport;

const EVENT_CAPACITY: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Command {
    pub id: u64,
    pub method: String,
    pub params: Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub struct Connection {
    inner: Arc<ConnectionInner>,
}

struct ConnectionInner {
    transport: tokio::sync::Mutex<Box<dyn Transport>>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    events: broadcast::Sender<Event>,
    closed: AtomicBool,
    closed_reason: Mutex<Option<String>>,
}

struct PendingRequest {
    method: String,
    completed: oneshot::Sender<Result<Value, CdpError>>,
}

impl Connection {
    pub fn new<T: Transport>(mut transport: T) -> Self {
        let incoming = transport.incoming();
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let inner = Arc::new(ConnectionInner {
            transport: tokio::sync::Mutex::new(Box::new(transport)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            events,
            closed: AtomicBool::new(false),
            closed_reason: Mutex::new(None),
        });
        tokio::spawn(read_messages(Arc::downgrade(&inner), incoming));
        Self { inner }
    }

    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        self.start_request(method, params, session_id).await?.await
    }

    pub async fn send_with_deadline(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        deadline: Duration,
    ) -> Result<Value, CdpError> {
        let (id, response) = self
            .start_request_with_id(method, params, session_id)
            .await?;
        match tokio::time::timeout(deadline, response).await {
            Ok(result) => result,
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(CdpError::Timeout)
            }
        }
    }

    /// Subscribes to all future CDP events.
    ///
    /// Create the receiver before sending the command that triggers an event.
    /// `broadcast::error::RecvError::Lagged` is returned if this receiver falls
    /// behind; callers must not silently discard it.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub fn session(&self, session_id: impl Into<String>) -> Session {
        Session {
            connection: self.clone(),
            session_id: session_id.into(),
        }
    }

    pub async fn close(&self) {
        self.inner.disconnect("closed by client");
        self.inner.transport.lock().await.close().await;
    }

    /// Returns why the connection closed, or `None` while it is live.
    pub fn closed_reason(&self) -> Option<String> {
        self.inner.closed_reason.lock().unwrap().clone()
    }

    async fn start_request(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<PendingResponse, CdpError> {
        let (_, response) = self
            .start_request_with_id(method, params, session_id)
            .await?;
        Ok(response)
    }

    async fn start_request_with_id(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<(u64, PendingResponse), CdpError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(CdpError::Disconnected);
        }

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let command = Command {
            id,
            method: method.to_owned(),
            params,
            session_id: session_id.map(str::to_owned),
        };
        let message = serde_json::to_string(&command)
            .map_err(|error| TransportError::InvalidMessage(error.to_string()))?;
        let (completed, response) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(
            id,
            PendingRequest {
                method: method.to_owned(),
                completed,
            },
        );

        if let Err(error) = self.inner.transport.lock().await.send(message).await {
            self.inner.pending.lock().unwrap().remove(&id);
            self.inner.disconnect(error.to_string());
            return Err(error.into());
        }

        Ok((id, PendingResponse { receiver: response }))
    }
}

struct PendingResponse {
    receiver: oneshot::Receiver<Result<Value, CdpError>>,
}

impl std::future::Future for PendingResponse {
    type Output = Result<Value, CdpError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match std::pin::Pin::new(&mut self.receiver).poll(context) {
            std::task::Poll::Ready(Ok(result)) => std::task::Poll::Ready(result),
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Err(CdpError::Disconnected)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl ConnectionInner {
    fn disconnect(&self, reason: impl Into<String>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.closed_reason.lock().unwrap() = Some(reason.into());
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (_, request) in pending {
            let _ = request.completed.send(Err(CdpError::Disconnected));
        }
    }
}

async fn read_messages(
    connection: Weak<ConnectionInner>,
    mut incoming: crate::transport::Incoming,
) {
    while let Some(message) = incoming.recv().await {
        let Some(connection) = connection.upgrade() else {
            return;
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                connection.disconnect(error.to_string());
                return;
            }
        };
        let parsed: Value = match serde_json::from_str(&message) {
            Ok(parsed) => parsed,
            Err(error) => {
                connection.disconnect(format!("invalid CDP message: {error}"));
                return;
            }
        };

        if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
            let request = connection.pending.lock().unwrap().remove(&id);
            if let Some(request) = request {
                let response = if let Some(error) = parsed.get("error") {
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
                    let server_message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown protocol error");
                    Err(CdpError::Protocol {
                        code,
                        message: format!("({}): {server_message}", request.method),
                        data: error.get("data").cloned(),
                    })
                } else {
                    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = request.completed.send(response);
            }
        } else if let Some(method) = parsed.get("method").and_then(Value::as_str) {
            let event = Event {
                method: method.to_owned(),
                params: parsed.get("params").cloned().unwrap_or_else(|| json!({})),
                session_id: parsed
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            let _ = connection.events.send(event);
        }
    }

    if let Some(connection) = connection.upgrade() {
        connection.disconnect("transport closed");
    }
}

#[derive(Clone)]
pub struct Session {
    connection: Connection,
    session_id: String,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub async fn send(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.connection
            .send(method, params, Some(&self.session_id))
            .await
    }

    pub async fn send_with_deadline(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value, CdpError> {
        self.connection
            .send_with_deadline(method, params, Some(&self.session_id), deadline)
            .await
    }

    /// Subscribes to future events for this session only.
    ///
    /// Create the receiver before sending the command that triggers an event.
    pub fn events(&self) -> SessionEvents {
        SessionEvents {
            session_id: self.session_id.clone(),
            receiver: self.connection.events(),
        }
    }
}

pub struct SessionEvents {
    session_id: String,
    receiver: broadcast::Receiver<Event>,
}

impl SessionEvents {
    pub async fn recv(&mut self) -> Result<Event, CdpError> {
        loop {
            match self.receiver.recv().await {
                Ok(event) if event.session_id.as_deref() == Some(&self.session_id) => {
                    return Ok(event);
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    return Err(CdpError::Lagged(count));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(CdpError::Disconnected);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;

    struct FakeTransport {
        sent: mpsc::Sender<String>,
        incoming: Option<crate::transport::Incoming>,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send(&self, message: String) -> Result<(), TransportError> {
            self.sent
                .send(message)
                .await
                .map_err(|_| TransportError::Closed)
        }

        fn incoming(&mut self) -> crate::transport::Incoming {
            self.incoming.take().unwrap()
        }

        async fn close(&self) {}
    }

    fn fake_connection() -> (
        Connection,
        mpsc::Receiver<String>,
        mpsc::Sender<Result<String, TransportError>>,
    ) {
        let (sent_tx, sent_rx) = mpsc::channel(16);
        let (incoming_tx, incoming_rx) = mpsc::channel(16);
        let connection = Connection::new(FakeTransport {
            sent: sent_tx,
            incoming: Some(incoming_rx),
        });
        (connection, sent_rx, incoming_tx)
    }

    #[tokio::test]
    async fn correlates_out_of_order_responses_and_protocol_errors() {
        let (connection, mut sent, incoming) = fake_connection();
        let first = tokio::spawn({
            let connection = connection.clone();
            async move { connection.send("First.call", json!({"a": 1}), None).await }
        });
        let second = tokio::spawn({
            let connection = connection.clone();
            async move { connection.send("Second.call", json!({}), None).await }
        });

        let one: Value = serde_json::from_str(&sent.recv().await.unwrap()).unwrap();
        let two: Value = serde_json::from_str(&sent.recv().await.unwrap()).unwrap();
        let (first_id, second_id) = if one["method"] == "First.call" {
            (one["id"].as_u64().unwrap(), two["id"].as_u64().unwrap())
        } else {
            (two["id"].as_u64().unwrap(), one["id"].as_u64().unwrap())
        };
        incoming
            .send(Ok(
                json!({"id": second_id, "result": {"value": 2}}).to_string()
            ))
            .await
            .unwrap();
        incoming
            .send(Ok(
                json!({"id": first_id, "result": {"value": 1}}).to_string()
            ))
            .await
            .unwrap();
        assert_eq!(first.await.unwrap().unwrap(), json!({"value": 1}));
        assert_eq!(second.await.unwrap().unwrap(), json!({"value": 2}));

        let bad = tokio::spawn({
            let connection = connection.clone();
            async move { connection.send("Target.bad", json!({}), None).await }
        });
        let command: Value = serde_json::from_str(&sent.recv().await.unwrap()).unwrap();
        incoming
            .send(Ok(json!({
                "id": command["id"],
                "error": {"code": -32000, "message": "No target", "data": "details"}
            })
            .to_string()))
            .await
            .unwrap();
        let error = bad.await.unwrap().unwrap_err();
        assert!(matches!(error, CdpError::Protocol { code: -32000, .. }));
        assert_eq!(error.to_string(), "Protocol error (Target.bad): No target");
    }

    #[tokio::test]
    async fn disconnect_fails_pending_requests() {
        let (connection, mut sent, incoming) = fake_connection();
        let pending = tokio::spawn({
            let connection = connection.clone();
            async move { connection.send("Never.answers", json!({}), None).await }
        });
        sent.recv().await.unwrap();
        drop(incoming);
        assert!(matches!(
            pending.await.unwrap(),
            Err(CdpError::Disconnected)
        ));
        assert_eq!(
            connection.closed_reason().as_deref(),
            Some("transport closed")
        );
    }
}
