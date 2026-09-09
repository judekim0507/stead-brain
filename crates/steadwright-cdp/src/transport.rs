use async_trait::async_trait;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::sync::{mpsc, oneshot, watch};

use crate::error::TransportError;

const MAX_MESSAGE_SIZE: usize = 256 * 1024 * 1024;
const READ_BUFFER_SIZE: usize = 64 * 1024;

pub type Incoming = mpsc::Receiver<Result<String, TransportError>>;

#[async_trait]
pub trait Transport: Send + 'static {
    async fn send(&self, message: String) -> Result<(), TransportError>;
    fn incoming(&mut self) -> Incoming;
    async fn close(&self);
}

struct Outgoing {
    message: String,
    completed: oneshot::Sender<Result<(), TransportError>>,
}

pub struct FdPairTransport {
    outgoing: mpsc::Sender<Outgoing>,
    incoming: Option<Incoming>,
    shutdown: watch::Sender<bool>,
}

impl FdPairTransport {
    /// Takes ownership of a read pipe fd and a write pipe fd.
    #[allow(unsafe_code)]
    pub fn from_raw_fds(read_fd: RawFd, write_fd: RawFd) -> Result<Self, TransportError> {
        // SAFETY: The caller transfers exclusive ownership of both valid pipe file
        // descriptors. OwnedFd closes each descriptor exactly once when dropped.
        let (read_fd, write_fd) = unsafe {
            (
                OwnedFd::from_raw_fd(read_fd),
                OwnedFd::from_raw_fd(write_fd),
            )
        };
        let reader = pipe::Receiver::from_owned_fd(read_fd)?;
        let writer = pipe::Sender::from_owned_fd(write_fd)?;
        Ok(Self::from_pipe_ends(reader, writer))
    }

    fn from_pipe_ends(reader: pipe::Receiver, writer: pipe::Sender) -> Self {
        let (incoming_tx, incoming) = mpsc::channel(128);
        let (outgoing, outgoing_rx) = mpsc::channel(128);
        let (shutdown, shutdown_rx) = watch::channel(false);

        tokio::spawn(read_asciiz_frames(reader, incoming_tx, shutdown_rx.clone()));
        tokio::spawn(write_asciiz_frames(writer, outgoing_rx, shutdown_rx));

        Self {
            outgoing,
            incoming: Some(incoming),
            shutdown,
        }
    }
}

#[async_trait]
impl Transport for FdPairTransport {
    async fn send(&self, message: String) -> Result<(), TransportError> {
        validate_outgoing(&message)?;
        let (completed, result) = oneshot::channel();
        self.outgoing
            .send(Outgoing { message, completed })
            .await
            .map_err(|_| TransportError::Closed)?;
        result.await.map_err(|_| TransportError::Closed)?
    }

    fn incoming(&mut self) -> Incoming {
        self.incoming
            .take()
            .expect("Transport::incoming may only be called once")
    }

    async fn close(&self) {
        let _ = self.shutdown.send(true);
    }
}

async fn read_asciiz_frames(
    mut reader: pipe::Receiver,
    incoming: mpsc::Sender<Result<String, TransportError>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut accumulated = Vec::new();
    let mut buffer = vec![0; READ_BUFFER_SIZE];
    // Bytes before this offset were already scanned for a terminator, so a
    // multi-megabyte frame arriving in 64 KiB reads stays linear.
    let mut scanned = 0usize;

    loop {
        let read = tokio::select! {
            _ = shutdown.changed() => break,
            result = reader.read(&mut buffer) => result,
        };
        let count = match read {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => {
                let _ = incoming.send(Err(error.into())).await;
                break;
            }
        };
        accumulated.extend_from_slice(&buffer[..count]);

        while let Some(offset) = accumulated[scanned..].iter().position(|byte| *byte == 0) {
            let end = scanned + offset;
            if end > MAX_MESSAGE_SIZE {
                let _ = incoming.send(Err(TransportError::FrameTooLarge)).await;
                return;
            }
            let remainder = accumulated.split_off(end + 1);
            accumulated.truncate(end);
            let frame = std::mem::replace(&mut accumulated, remainder);
            scanned = 0;
            match String::from_utf8(frame) {
                Ok(message) => {
                    if incoming.send(Ok(message)).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = incoming.send(Err(error.into())).await;
                    return;
                }
            }
        }

        scanned = accumulated.len();

        if accumulated.len() > MAX_MESSAGE_SIZE {
            let _ = incoming.send(Err(TransportError::FrameTooLarge)).await;
            break;
        }
    }
}

async fn write_asciiz_frames(
    mut writer: pipe::Sender,
    mut outgoing: mpsc::Receiver<Outgoing>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let outgoing = tokio::select! {
            _ = shutdown.changed() => break,
            outgoing = outgoing.recv() => outgoing,
        };
        let Some(outgoing) = outgoing else {
            break;
        };
        let result: std::io::Result<()> = async {
            writer.write_all(outgoing.message.as_bytes()).await?;
            writer.write_all(&[0]).await?;
            writer.flush().await?;
            Ok(())
        }
        .await;
        let failed = result.is_err();
        let _ = outgoing
            .completed
            .send(result.map_err(TransportError::from));
        if failed {
            break;
        }
    }
}

fn validate_outgoing(message: &str) -> Result<(), TransportError> {
    if message.len() > MAX_MESSAGE_SIZE {
        return Err(TransportError::FrameTooLarge);
    }
    if message.as_bytes().contains(&0) {
        return Err(TransportError::EmbeddedNul);
    }
    Ok(())
}

#[cfg(feature = "ws")]
pub struct WebSocketTransport {
    outgoing: mpsc::Sender<Outgoing>,
    incoming: Option<Incoming>,
    shutdown: watch::Sender<bool>,
}

#[cfg(feature = "ws")]
impl WebSocketTransport {
    pub async fn connect(url: &str) -> Result<Self, TransportError> {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_SIZE))
            .max_frame_size(Some(MAX_MESSAGE_SIZE));
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(url, Some(config), false).await?;
        let (mut sink, mut stream) = socket.split();
        let (incoming_tx, incoming) = mpsc::channel(128);
        let (outgoing, mut outgoing_rx) = mpsc::channel::<Outgoing>(128);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let mut read_shutdown = shutdown_rx.clone();
        let mut write_shutdown = shutdown_rx;

        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = read_shutdown.changed() => break,
                    message = stream.next() => message,
                };
                match message {
                    Some(Ok(Message::Text(text))) => {
                        if incoming_tx.send(Ok(text.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let _ = incoming_tx
                            .send(Err(TransportError::InvalidMessage(
                                "received a binary WebSocket frame; CDP requires text".into(),
                            )))
                            .await;
                        break;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        let _ = incoming_tx.send(Err(error.into())).await;
                        break;
                    }
                }
            }
        });

        tokio::spawn(async move {
            loop {
                let outgoing = tokio::select! {
                    _ = write_shutdown.changed() => {
                        let _ = sink.close().await;
                        break;
                    },
                    outgoing = outgoing_rx.recv() => outgoing,
                };
                let Some(outgoing) = outgoing else {
                    let _ = sink.close().await;
                    break;
                };
                let result = sink
                    .send(Message::Text(outgoing.message.into()))
                    .await
                    .map_err(TransportError::from);
                let failed = result.is_err();
                let _ = outgoing.completed.send(result);
                if failed {
                    break;
                }
            }
        });

        Ok(Self {
            outgoing,
            incoming: Some(incoming),
            shutdown,
        })
    }
}

#[cfg(feature = "ws")]
#[async_trait]
impl Transport for WebSocketTransport {
    async fn send(&self, message: String) -> Result<(), TransportError> {
        validate_outgoing(&message)?;
        let (completed, result) = oneshot::channel();
        self.outgoing
            .send(Outgoing { message, completed })
            .await
            .map_err(|_| TransportError::Closed)?;
        result.await.map_err(|_| TransportError::Closed)?
    }

    fn incoming(&mut self) -> Incoming {
        self.incoming
            .take()
            .expect("Transport::incoming may only be called once")
    }

    async fn close(&self) {
        let _ = self.shutdown.send(true);
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::IntoRawFd;

    use super::*;

    #[tokio::test]
    async fn asciiz_framing_handles_boundaries_empty_and_large_frames() {
        let (browser_writer, client_reader) = pipe::pipe().unwrap();
        let (client_writer, mut browser_reader) = pipe::pipe().unwrap();
        let read_fd = client_reader.into_blocking_fd().unwrap().into_raw_fd();
        let write_fd = client_writer.into_blocking_fd().unwrap().into_raw_fd();
        let mut transport = FdPairTransport::from_raw_fds(read_fd, write_fd).unwrap();
        let mut incoming = transport.incoming();

        let large = "x".repeat(5 * 1024 * 1024);
        let expected_large = large.clone();
        tokio::spawn(async move {
            let mut browser_writer = browser_writer;
            browser_writer.write_all(b"one\0split").await.unwrap();
            browser_writer.write_all(b"-frame\0\0").await.unwrap();
            browser_writer.write_all(large.as_bytes()).await.unwrap();
            browser_writer.write_all(&[0]).await.unwrap();
        });

        assert_eq!(incoming.recv().await.unwrap().unwrap(), "one");
        assert_eq!(incoming.recv().await.unwrap().unwrap(), "split-frame");
        assert_eq!(incoming.recv().await.unwrap().unwrap(), "");
        assert_eq!(incoming.recv().await.unwrap().unwrap(), expected_large);

        transport.send("response".into()).await.unwrap();
        let mut response = vec![0; b"response\0".len()];
        browser_reader.read_exact(&mut response).await.unwrap();
        assert_eq!(response, b"response\0");
    }
}
