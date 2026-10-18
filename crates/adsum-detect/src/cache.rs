//! Memoized per-pid Steam AppIds with invalidation sequences.
//!
//! Environ reads are kilobytes and never change after exec: one read per
//! process lifetime, discarded (never stored) when an EXEC races it.

use rustc_hash::FxHashSet;

use crate::scan::{ExecScratch, read_steam_app_id};
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
  /// reuse must never serve a stale id. One set build + retain per tick.
  /// Recovers from a poisoned lock like every other cache access (a dead
  /// pid set is still better than no sweep at all).
  ///
  /// Capacity is trimmed when the live set collapses (a build storm's
  /// thousands of pids dropping back to a few hundred): `retain` alone
  /// keeps the high-water bucket count, so the table would pin that
  /// memory for the rest of the session. `len`/`capacity` are read before
  /// the lock so the rehash happens outside it.
  pub(crate) fn sweep_dead_appids(&self, processes: &[Exec]) {
    let mut live = FxHashSet::with_capacity_and_hasher(processes.len(), Default::default());
    live.extend(processes.iter().map(|process| process.pid));

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
mod sweep_tests {
  use super::*;

  use adsum_telemetry::QueueGauge;
  /// A bare server: no detectable base, no listeners, no refresh.
  fn server() -> ProcessServer {
    let (tx, _rx) = QueueGauge::pair();
    ProcessServer::new_with_custom(
      vec![],
      vec![],
      tx,
      crate::types::ProcessEventListeners::default(),
      crate::refresh::RefreshConfig::default(),
      vec![],
    )
  }

  /// An `Exec` carrying only the pid (the sweep's sole input).
  fn exec(pid: u64) -> Exec {
    Exec {
      pid,
      path: String::new(),
      arguments: None,
    }
  }

  /// One memoized pid, as a live scan would leave it.
  fn memoize(server: &ProcessServer, pid: u64) {
    let mut cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.insert(pid, AppIdMemo::Absent(1));
  }

  /// A spike then a collapse: the table must not sit at its high-water
  /// bucket count for the rest of the session (`retain` alone keeps it).
  #[test]
  fn sweep_releases_spike_capacity_after_a_collapse() {
    let server = server();

    // Spike: well past the shrink threshold.
    let spike: Vec<u64> = (0..500u64).collect();
    for pid in &spike {
      memoize(&server, *pid);
    }
    let spiked_capacity = server
      .appid_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .capacity();
    assert!(
      spiked_capacity > 64,
      "spike should have grown the table, got {spiked_capacity}"
    );

    // Collapse: most pids die, the sweep keeps only the survivors.
    let survivors: Vec<u64> = spike[..20].to_vec();
    server.sweep_dead_appids(&survivors.iter().map(|pid| exec(*pid)).collect::<Vec<_>>());

    let cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(cache.len(), survivors.len());
    // Capacity must follow the live set, not the spike: the release is the
    // whole point, so an assertion that holds either way proves nothing.
    assert!(
      cache.capacity() <= cache.len() * 4 + 16,
      "capacity {} should have tracked {} entries, not the spike of {}",
      cache.capacity(),
      cache.len(),
      spiked_capacity
    );
    assert!(
      cache.capacity() < spiked_capacity / 2,
      "capacity {} barely moved off the spike {}",
      cache.capacity(),
      spiked_capacity
    );
  }

  /// Dead pids are dropped and live ones survive — the sweep's actual job,
  /// which capacity trimming must never get in the way of.
  #[test]
  fn sweep_drops_dead_pids_and_keeps_live_ones() {
    let server = server();
    for pid in [10u64, 20, 30] {
      memoize(&server, pid);
    }

    server.sweep_dead_appids(&[exec(10), exec(30)]);

    let cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
    assert!(cache.contains_key(&10));
    assert!(!cache.contains_key(&20), "dead pid must be dropped");
    assert!(cache.contains_key(&30));
  }

  /// D7: the memo holds a refcounted id, so a cache hit is a refcount
  /// bump instead of a `String` clone.
  ///
  /// Every monitored game paid one `String` copy per scan tick — the
  /// `.cloned()` of the memo plus the copy into the return value. With
  /// `AppId` inside, both are counter bumps over the same buffer.
  ///
  /// The invariant is pointer identity: a clone that copies would land
  /// in a fresh allocation and this fails, while a refcount bump does not.
  #[test]
  fn cached_app_id_hit_shares_the_memoized_buffer() {
    let server = server();
    // Plant a verified memo directly, so the assertion is about the hit
    // path (no environment dependency on whether /proc/<pid>/environ
    // happens to carry a SteamAppId).
    let pid = 42u64;
    {
      let mut cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
      cache.insert(
        pid,
        AppIdMemo::Present(1, AppId::from("123456789012345678")),
      );
    }
    let stored_ptr = {
      let cache = server.appid_cache.lock().unwrap_or_else(|e| e.into_inner());
      let Some(AppIdMemo::Present(_, id)) = cache.get(&pid) else {
        panic!("planted memo must be Present");
      };
      id.as_ptr()
    };

    let mut scratch = ExecScratch::default();
    let returned = server.cached_app_id(pid, &mut scratch).expect("hit");

    assert!(
      std::ptr::eq(returned.as_ptr(), stored_ptr),
      "a cache hit must share the memoized buffer, never copy it"
    );
    assert_eq!(&*returned, "123456789012345678");
  }
}
