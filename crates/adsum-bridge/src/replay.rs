//! Replay cache: last payload per socket id, rebroadcast to late
//! joiners and revisited on refresh.
//!
//! Bounded by [`MAX_CACHED_ACTIVITIES`][crate::config::MAX_CACHED_ACTIVITIES]:
//! eviction drops the oldest sequence first. Pids resolve out of payload
//! bodies for ghost reaping (see [`cache_entry_pid`]).

use std::collections::HashMap;
use std::sync::Arc;

use adsum_protocol::commands::CachedActivity;
use adsum_types::SocketId;

/// Replay cache: socket id → (shared payload, sequence).
pub(crate) type ReplayCache = HashMap<SocketId, (Arc<CachedActivity>, u64)>;

/// Evict the oldest entries while the replay cache exceeds
/// [`MAX_CACHED_ACTIVITIES`][crate::config::MAX_CACHED_ACTIVITIES].
/// Pure map operation (no locks taken here).
pub(crate) fn prune_cache(cache: &mut ReplayCache) {
  while cache.len() > crate::config::MAX_CACHED_ACTIVITIES {
    let oldest = cache
      .iter()
      .min_by_key(|(_, (_, seq))| *seq)
      .map(|(key, _)| key.clone());
    match oldest {
      Some(key) => {
        cache.remove(&key);
      }
      None => break,
    }
  }
}

/// Resolve the owning pid of a replay-cache entry for ghost reaping: the
/// pid the builder recorded, falling back to numeric socket ids (SDK
/// connections use the pid verbatim).
///
/// The recorded pid is authoritative because generic scanner cards are
/// cached under the numeric application id: resolving the socket id
/// first would mistake the app id for a dead pid and evict live generic
/// cards on every null sweep.
///
/// Returns `None` when neither yields a usable pid — pid 0 included:
/// unidentifiable publishers can never be proven dead, and clearing
/// them risks darkening a live-but-broken client (same convention as
/// the private `is_genuine_clear`).
///
/// A payload built before the pid field existed (or with no pid at all)
/// records 0 and falls through to the socket id, which is the same place
/// an unusable body pid used to land.
pub fn cache_entry_pid(socket_id: &SocketId, payload: &CachedActivity) -> Option<u64> {
  if payload.pid != 0 {
    return Some(payload.pid);
  }
  if let Ok(pid) = socket_id.as_ref().parse::<u64>() {
    return (pid != 0).then_some(pid);
  }
  None
}

#[cfg(test)]
#[path = "../tests/unit/replay.rs"]
mod tests;
