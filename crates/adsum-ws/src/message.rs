//! Wire message with cheap clone for both payload kinds.

use bytes::Bytes;
use tungstenite::Utf8Bytes;

/// Re-exported so consumers can name the text backing without depending
/// on `tungstenite` directly.
pub use tungstenite::Utf8Bytes as TextBytes;

/// An incoming/outgoing WebSocket message.
///
/// Both variants clone in O(1): [`Bytes`] and [`Utf8Bytes`] are refcounted,
/// so broadcast fan-out shares the allocation instead of copying per
/// client (`mem-zero-copy`). Builders that already own a `String`/`Vec<u8>`
/// hand the allocation over with no copy (`From` impls below).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
  /// UTF-8 text message.
  Text(Utf8Bytes),
  /// Binary message (zero-copy clone).
  Binary(Bytes),
}

impl Message {
  /// Length in bytes of the payload.
  #[must_use]
  pub fn len(&self) -> usize {
    match self {
      Self::Text(s) => s.len(),
      Self::Binary(b) => b.len(),
    }
  }

  /// True when the payload is empty.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

impl From<String> for Message {
  /// Adopt an owned string as text (no copy).
  fn from(s: String) -> Self {
    Self::Text(s.into())
  }
}

impl From<&str> for Message {
  /// Copy a borrowed string as text.
  fn from(s: &str) -> Self {
    Self::Text(s.into())
  }
}

impl From<Utf8Bytes> for Message {
  /// Adopt refcounted text without copying.
  fn from(s: Utf8Bytes) -> Self {
    Self::Text(s)
  }
}

impl From<Bytes> for Message {
  /// Adopt refcounted bytes without copying.
  fn from(b: Bytes) -> Self {
    Self::Binary(b)
  }
}

impl From<Vec<u8>> for Message {
  /// Adopt an owned byte vector as binary (no copy).
  fn from(v: Vec<u8>) -> Self {
    Self::Binary(Bytes::from(v))
  }
}

#[cfg(test)]
#[path = "../tests/unit/message.rs"]
mod tests;
