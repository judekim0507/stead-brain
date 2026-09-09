use serde_json::Value;

/// Errors produced by a CDP connection.
#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("Protocol error {message}")]
    Protocol {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("CDP connection disconnected")]
    Disconnected,
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("CDP event receiver lagged by {0} events")]
    Lagged(u64),
    #[error("CDP request timed out")]
    Timeout,
}

/// Errors produced by a CDP transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "ws")]
    #[error("WebSocket error: {0}")]
    WebSocket(#[source] Box<tokio_tungstenite::tungstenite::Error>),
    #[error("CDP frame exceeds the 256 MiB limit")]
    FrameTooLarge,
    #[error("CDP frame contains invalid UTF-8: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("outgoing CDP message contains an embedded NUL byte")]
    EmbeddedNul,
    #[error("invalid CDP message: {0}")]
    InvalidMessage(String),
    #[error("transport is closed")]
    Closed,
}

#[cfg(feature = "ws")]
impl From<tokio_tungstenite::tungstenite::Error> for TransportError {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(error))
    }
}
