//! Newtyped ids: [`AppId`] (game/activity slot) and [`SocketId`] (client slot).
//!
//! `Arc<str>` inside (`own-arc-shared`): clones share the allocation and
//! `Borrow<str>` map lookups never allocate on the hot path. Same JSON wire
//! format as a plain string.

use std::sync::Arc;

/// Discord application id: identifies a game/activity slot. Newtyped so
/// socket ids, pids and raw strings can never mix at compile time; same
/// wire format as the inner string.
#[derive(
  Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
pub struct AppId(pub Arc<str>);

/// Bridge socket id: identifies one client connection slot. See [`AppId`].
#[derive(
  Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[repr(transparent)]
pub struct SocketId(pub Arc<str>);

impl std::fmt::Display for AppId {
  /// Format the inner id verbatim (wire format).
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    self.0.fmt(f)
  }
}

impl std::fmt::Display for SocketId {
  /// Format the inner id verbatim (wire format).
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    self.0.fmt(f)
  }
}

impl AsRef<str> for AppId {
  /// Borrow the inner id as `str` (no allocation, no copy).
  fn as_ref(&self) -> &str {
    &self.0
  }
}

/// Borrow the id as its `str`: the newtype is a transparent wrapper over
/// `Arc<str>`, so deref costs a pointer hop and never a copy.
///
/// This is what lets `AppId` stand in for `&str` everywhere the pipeline
/// already expects one — `Option<AppId>::as_deref()`, `id.starts_with(…)`
/// — instead of every call site spelling out a conversion.
impl std::ops::Deref for AppId {
  type Target = str;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

/// Borrow as `str` so `HashMap<AppId, _>` lookups accept `&str` without
/// allocating an owned key on the hot path.
impl std::borrow::Borrow<str> for AppId {
  /// Borrow for `HashMap` str lookups (no owned key).
  fn borrow(&self) -> &str {
    &self.0
  }
}

/// Same as [`AppId`]: socket maps accept `&str` lookups directly.
impl std::borrow::Borrow<str> for SocketId {
  /// Borrow for `HashMap` str lookups (no owned key).
  fn borrow(&self) -> &str {
    &self.0
  }
}

impl AsRef<str> for SocketId {
  /// Borrow the inner id as `str` (no allocation, no copy).
  fn as_ref(&self) -> &str {
    &self.0
  }
}

/// Same as [`AppId`], whose `Deref` target is also `str`: the socket id is
/// a transparent wrapper over `Arc<str>`, so it stands in for `&str` the
/// same way.
impl std::ops::Deref for SocketId {
  type Target = str;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl From<String> for AppId {
  /// Adopt an owned string without revalidating (ids are opaque).
  fn from(id: String) -> Self {
    Self(id.into())
  }
}

impl From<Box<str>> for AppId {
  /// Adopt a boxed slice (one copy into the refcounted layout).
  fn from(id: Box<str>) -> Self {
    Self(Arc::from(id))
  }
}

impl From<&str> for AppId {
  /// Copy a borrowed id into shared ownership.
  fn from(id: &str) -> Self {
    Self(Arc::from(id))
  }
}

impl From<String> for SocketId {
  /// Adopt an owned string without revalidating (ids are opaque).
  fn from(id: String) -> Self {
    Self(id.into())
  }
}

impl From<Box<str>> for SocketId {
  /// Adopt a boxed slice (one copy into the refcounted layout).
  fn from(id: Box<str>) -> Self {
    Self(Arc::from(id))
  }
}

impl From<&str> for SocketId {
  /// Copy a borrowed id into shared ownership.
  fn from(id: &str) -> Self {
    Self(Arc::from(id))
  }
}

/// An app slot doubles as its own socket on the generic (scanner-driven)
/// path: move the id across instead of cloning it.
impl From<AppId> for SocketId {
  /// Move the allocation across (no clone, no copy).
  fn from(id: AppId) -> Self {
    Self(id.0)
  }
}

/// A numeric pid renders straight into the refcounted string, with no
/// heap-allocated `String` in between (pids are the socket on the SDK
/// path, and the decimal used to be built twice per publish).
impl From<u64> for SocketId {
  /// Format the pid as decimal without a temporary `String`.
  fn from(pid: u64) -> Self {
    // Widest decimal a u64 renders to: u64::MAX has 20 digits.
    const DECIMAL_CAP: usize = 20;
    let mut buf = [0u8; DECIMAL_CAP];
    let mut cursor = buf.len();
    let mut rest = pid;
    loop {
      cursor -= 1;
      buf[cursor] = b'0' + (rest % 10) as u8;
      rest /= 10;
      if rest == 0 {
        break;
      }
    }
    // Only ASCII digits ever reach the buffer, so this cannot fail.
    let digits = std::str::from_utf8(&buf[cursor..]).expect("ascii digits");
    Self(Arc::from(digits))
  }
}

/// Borrow an app id as its socket without touching the inner string.
impl From<&AppId> for SocketId {
  /// Share the allocation (refcount bump, no string copy).
  fn from(id: &AppId) -> Self {
    Self(id.0.clone())
  }
}

/// Unwrap back to the wire string (Display also works for formatting).
impl From<AppId> for String {
  /// Copy out to an owned wire string.
  fn from(id: AppId) -> Self {
    id.0.to_string()
  }
}

/// Unwrap back to the wire string (Display also works for formatting).
impl From<SocketId> for String {
  /// Copy out to an owned wire string.
  fn from(id: SocketId) -> Self {
    id.0.to_string()
  }
}

#[cfg(test)]
#[path = "../tests/unit/app_id.rs"]
mod tests;
