//! Memoized per-pid Steam AppIds with invalidation sequences.
//!
//! Environ reads are kilobytes and never change after exec: one read per
//! process lifetime, discarded (never stored) when an EXEC races it.

use rustc_hash::FxHashSet;

use crate::platform::read_steam_app_id;
use crate::scratch::ExecScratch;
use crate::server::ProcessServer;
use crate::types::Exec;
use adsum_types::AppId;

/// Memoized SteamAppId plus its invalidation sequence (see
/// [`ProcessServer::cached_app_id`]). Verified outcomes (`Present` /
/// `Absent`) cost zero I/O until EXEC invalidation or pid death; only
/// `Stale` reads environ. Distinguishing `Absent` from `Stale` is the
/// whole point: nearly every desktop process has no SteamAppId, and
/// re-reading kilobytes of environ for all of them every tick was the
/// biggest per-process I/O cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AppIdMemo {
  /// Verified: this pid has no SteamAppId. Valid until invalidated.
  Absent(u64),
  /// Verified SteamAppId, valid under the same rules. Refcounted, so a
  /// cache hit is a counter bump rather than a `String` copy per tick.
  Present(u64, AppId),
  /// Never read, or invalidated by EXEC: read environ, then store the
  /// outcome — unless the sequence moved under us (racing EXEC), in
  /// which case the stale read is discarded, never stored.
  /// (Constructed only by the Linux-only `drop_appid`; other platforms
  /// would flag the variant as dead without the allow.)
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  Stale(u64),
}

impl ProcessServer {
  /// SteamAppId for one pid, memoized: environ is kilobytes and never
  /// changes after exec, so re-reading it every 5s per process was pure
  /// waste (profiler: ~5KB of the ~5KB per-process cost). IO happens
  /// outside the lock; EXEC invalidates via [`ProcessServer::drop_appid`],
  /// whose sequence bump discards a racing stale read below.
  /// (`pub(crate)` for the memo-semantics regression test.)
  ///
  /// `exec_scratch` supplies the environ and path buffers, so a
  /// `Stale` miss (the common case) costs no allocation of its own.
  pub fn cached_app_id(&self, pid: u64, exec_scratch: &mut ExecScratch) -> Option<AppId> {
    // Fast paths: verified memos, cloned once straight into the return.
    // `Absent` is the common case (non-Steam processes) and performs no
    // I/O at all; only `Stale` falls through to the environ read below.
    // The memo's `AppId` clone is a refcount bump, not a copy of the id.
    let memo = self
      .appid_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(&pid)
      .cloned();
    let seq0 = match memo {
      Some(AppIdMemo::Present(_, id)) => return Some(id),
      Some(AppIdMemo::Absent(_)) => return None,
      Some(AppIdMemo::Stale(seq)) => seq,
      None => 0,
    };
    let id = read_steam_app_id(pid, exec_scratch);
    let mut cache = self.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
    // A racing EXEC invalidated this pid mid-read (sequence bumped) or
    // the tick swept it: discard the stale environ instead of pinning
    // a pre-exec id for the pid lifetime.
    let fresh = match cache.get(&pid) {
      Some(AppIdMemo::Stale(seq)) if *seq == seq0 => true,
      None if seq0 == 0 => true,
      _ => false,
    };
    if fresh {
      let memo = match id.as_ref() {
        Some(id) => AppIdMemo::Present(seq0, AppId::from(id.as_str())),
        None => AppIdMemo::Absent(seq0),
      };
      cache.insert(pid, memo);
    }
    // The miss path still owns a `String` from the environ read; wrap it
    // for the caller rather than changing the reader's shape. A hit that
    // reached here (not `fresh`) re-wraps its own `String` once.
    id.map(AppId::from)
  }

  /// Drop memoized AppIds of pids that died since the last tick: pid
  /// reuse must never serve a stale id. The liveness set is the
  /// caller's reuse buffer (cleared and refilled per tick — the sweep
  /// used to build a fresh `FxHashSet` every tick, one allocation per
  /// tick for a table that only ever lives for the call).
  ///
  /// `Exec`-table convenience wrapper for the sweeps that have a filled
  /// process list at hand (the full-sweep mode on every target, and the
  /// tests); the incremental mode derives the set from the enumerated
  /// pids and calls the `_by_live` form directly.
  pub(crate) fn sweep_dead_appids(&self, processes: &[Exec], live: &mut FxHashSet<u64>) {
    live.clear();
    live.extend(processes.iter().map(|process| process.pid));
    self.sweep_dead_appids_by_live(live);
  }

  /// The sweep proper, against an already-built liveness set. Recovers
  /// from a poisoned lock like every other cache access (a dead pid set
  /// is still better than no sweep at all).
  ///
  /// Capacity is trimmed when the live set collapses (a build storm's
  /// thousands of pids dropping back to a few hundred): `retain` alone
  /// keeps the high-water bucket count, so the table would pin that
  /// memory for the rest of the session. `len`/`capacity` are read before
  /// the lock so the rehash happens outside it.
  pub(crate) fn sweep_dead_appids_by_live(&self, live: &FxHashSet<u64>) {
    let mut cache = self.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|pid, _| live.contains(pid));
    let (len, capacity) = (cache.len(), cache.capacity());
    drop(cache);

    if capacity > 64 && len * 4 < capacity {
      let target = (len * 2).max(16);
      let mut cache = self.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
      // Only re-lock when the target is still worth a rehash: another
      // sweep may have run in between and moved the numbers.
      if cache.len() * 4 < cache.capacity() {
        cache.shrink_to(target);
      }
    }
  }

  /// Drop one pid's memoized AppId (EXEC: same pid, new image, possibly
  /// new environ). Called by the proc-events watcher before reclassifying.
  /// Bumps the sequence so an in-flight [`ProcessServer::cached_app_id`]
  /// read for the old image is discarded, never stored.
  #[cfg(target_os = "linux")]
  pub fn drop_appid(&self, pid: u64) {
    self
      .appid_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .entry(pid)
      .and_modify(|entry| {
        let seq = match entry {
          AppIdMemo::Absent(seq) | AppIdMemo::Present(seq, _) | AppIdMemo::Stale(seq) => *seq,
        };
        *entry = AppIdMemo::Stale(seq.wrapping_add(1));
      })
      .or_insert(AppIdMemo::Stale(1));
  }
}

#[cfg(test)]
#[path = "../../tests/unit/cache/appid.rs"]
mod tests;
