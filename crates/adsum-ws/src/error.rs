//! Typed errors for [`crate::Server`] and send paths.

use std::io;

/// Error returned by server setup paths.
///
/// Every variant preserves its cause: [`Error::Bind`] keeps the OS error so
/// callers can match `AddrInUse`, unlike the old opaque `FailedToStart`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
  /// TCP bind failed.
  #[error("failed to bind websocket listener: {0}")]
  Bind(#[source] io::Error),
  /// Runtime or socket-option setup failed after a successful bind.
  #[error("failed to start websocket runtime: {0}")]
  Runtime(#[source] io::Error),
  /// HTTP upgrade was not a valid WebSocket handshake.
  #[error("invalid websocket handshake")]
  Handshake,
  /// A [`crate::ServerConfig`] value was rejected.
  #[error("invalid server config: {0}")]
  Config(&'static str),
}

/// Send failed because the client is gone.
///
/// Returned by `Responder::send_async`; maps from [`TrySendError`] so both
/// backpressure (`Full`) and disconnect (`Closed`) collapse to "not delivered".
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("client is disconnected")]
pub struct SendError;

/// `try_send` failure, distinguishing backpressure from disconnect.
///
/// Callers in poll loops use this to decide between "retry later" (`Full`)
/// and "drop the responder" (`Closed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TrySendError {
  /// Client queue full (slow consumer); retry or drop per caller policy.
  #[error("client queue full")]
  Full,
  /// Client is disconnected.
  #[error("client is disconnected")]
  Closed,
}

impl From<TrySendError> for SendError {
  /// Collapse both `Full` and `Closed` into "not delivered".
  fn from(_: TrySendError) -> Self {
    Self
  }
}

#[cfg(test)]
#[path = "../tests/unit/error.rs"]
mod tests;
