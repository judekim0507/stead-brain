#![deny(unsafe_code)]

//! Transport-agnostic Chrome DevTools Protocol primitives.

pub mod attach;
pub mod chromium;
pub mod connection;
pub mod error;
pub mod transport;

pub use attach::{TargetId, TargetInfo, TargetTracker, auto_attach};
pub use connection::{Command, Connection, Event, Session, SessionEvents};
pub use error::{CdpError, TransportError};
#[cfg(feature = "ws")]
pub use transport::WebSocketTransport;
pub use transport::{FdPairTransport, Transport};
