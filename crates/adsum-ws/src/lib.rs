//! Modern async WebSocket server with bounded queues and graceful shutdown.
//!
//! `adsum-ws` replaces the 2021-era vendored `simple-websockets` crate:
//! every queue is bounded, no lock is held across `.await`, and
//! [`Server::shutdown`] terminates all connection tasks deterministically.
//! A client whose outbox stays full is reported to the caller as
//! [`TrySendError::Full`](Responder::try_send) — the server never grows
//! memory to absorb it and never disconnects it on its own; pruning is the
//! caller's decision.
//!
//! The primary consumption pattern is an async event loop over [`EventHub`].

pub mod config;
pub mod error;
pub mod hub;
pub mod message;
pub mod server;

pub use config::{ServerConfig, ServerConfigBuilder};
pub use error::{Error, SendError, TrySendError};
pub use hub::{CloseCode, ConnectionDetails, DisconnectReason, Event, EventHub, Responder};
pub use message::{Message, TextBytes};
pub use server::{MAX_PENDING_REJECTS, Server};

/// Opaque client identifier, unique within the process lifetime.
///
/// Allocated from an [`std::sync::atomic::AtomicU64`]; values are never reused
/// while the server lives, so a `Disconnect` always refers to the connection
/// its `Connect` announced.
pub type ClientId = u64;
