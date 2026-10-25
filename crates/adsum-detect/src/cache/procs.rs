//! Per-pid classification cache: the incremental scan mode.
//!
//! The periodic sweep used to re-read every process's `cmdline` and
//! re-run the whole match pipeline every tick, even though the netlink
//! watcher already observes every `EXEC`/`EXIT` in between. This cache
//! closes that redundancy: a pid's classification is computed once — by
//! the EXEC fast path or by the tick that first sees the pid — and later
//! ticks only ENUMERATE `/proc` (liveness truth), sweep the dead, and
//! re-read the pids whose entry is missing or was classified against a
//! foreign bundle generation.
//!
//! # Trust model (why this is sound)
//!
//! Liveness never comes from the cache: every tick enumerates the live
//! pid set and sweeps entries against it, exactly like the AppId memo's
//! dead-pid sweep. What the cache asserts is narrower — "this pid has
//! not `exec`'d since it was classified" — and that rests on the same
//! watcher the EXIT-wake and idle-backoff paths already trust, with
//! every known loss channel mapped to a full invalidation:
//!
//! | Signal | Source | Effect |
//! |---|---|---|
//! | `EXEC(pid)` | dispatch thread | entry replaced with the fresh classification (before the existing publish) |
//! | `EXIT(pid)` | dispatch thread | entry removed (unconditionally — dead is dead) |
//! | zombie between ticks | the tick itself | cached HITS are revalidated every tick (one cmdline read each, 0-3 of them): a zombie is still enumerated but reads empty, and "unreadable = absent" is the full sweep's rule, so it drops its entry at once |
//! | cn_proc sequence gap | watcher (`SeqTracker`) | `cache_dirty` → next tick flushes: one full-cost re-read, self-healing |
//! | backlog shed (>cap) | watcher (`send_checked`) | same flag: a shed event is a lost invalidation |
//! | socket overrun (ENOBUFS) | watcher (`recv`) | same flag |
//! | watcher death / (re)subscribe | watcher retry loop | same flag on BOTH edges: the death window's events were never seen, and the resubscribe window's were never buffered |
//! | bundle swap (refresh/override) | `DetectablesBundle::epoch` | every entry carries the epoch it was classified against; a foreign epoch makes the pid "work" again, so the swap costs exactly one full-read tick per hour |
//! | watcher disabled or not live | `cache_mode_live` | incremental mode is off: the tick runs the classic full sweep (the pre-cache behavior, unchanged) |
//!
//! The worst case of any combination of losses is one tick at the old
//! cost — never a wrong answer that survives a tick, because the flush
//! re-reads `cmdline` for every live pid and the sweep re-derives
//! liveness from `/proc` itself.
//!
//! # Memory
//!
//! One small entry per live pid (~56 B + bucket): ~60 KB at 1k
//! processes, bounded by the live set (swept every tick) and shrunk
//! after a collapse like the AppId memo. The cache REPLACES per-tick
//! work, not the slot list: the `Vec<Exec>` high-water slots survive
//! (the full-sweep fallback and work re-reads refill them).

use rustc_hash::FxHashMap;
use std::sync::atomic::Ordering;

use crate::server::ProcessServer;
use crate::types::ScannedHit;

/// One cached classification for a live pid.
pub(crate) struct CachedProc {
  /// The bundle generation this was classified against: a foreign epoch
  /// (hourly refresh, override edit) sends the pid back through the
  /// reader and matcher — see the module docs.
  pub(crate) epoch: u64,
  /// The classification: `None` is a verified miss (excluded, refused,
  /// or simply not a game) and skips the pid exactly like a fresh miss
  /// would. Hits carry the shared `Arc<ScannedEntry>`, so a tick's
  /// table is refcount bumps, never copies.
  pub(crate) hit: Option<ScannedHit>,
  /// The OBS/streamlabs path flag observed at classification time: the
  /// tick's `obs_open` is the OR of the live entries' bits, matching
  /// what a full sweep would have computed from the same paths.
  pub(crate) obs: bool,
}

/// The cache table: pid → classification. `Mutex`, not `RwLock`: the
/// readers (sweep, work collection, table build) all mutate stamps or
/// hold the lock for microseconds-long walks, and the writers (dispatch
/// EXEC/EXIT, work inserts) are single-entry ops — a read lock would add
/// a writer-starvation question for no gain.
pub(crate) type ProcCache = FxHashMap<u64, CachedProc>;

impl ProcessServer {
  /// Whether the next tick may run incremental: the watcher must be
  /// enabled AND believed live (socket up). Every polling-only path —
  /// targets without a watcher (only Linux has one), `--no-proc-events`,
  /// failed setups, retry windows — keeps the classic full sweep, so a
  /// dead watcher can never leave a classification trusted past its
  /// tick.
  pub(crate) fn cache_mode_live(&self) -> bool {
    self.enable_proc_events && self.watcher_live.load(Ordering::Acquire)
  }

  /// Consume the invalidation flag: `true` means the watcher saw a loss
  /// channel (gap/shed/overrun/restart) since the last tick, and the
  /// cache must be flushed before being trusted again.
  pub(crate) fn take_cache_dirty(&self) -> bool {
    self.cache_dirty.swap(false, Ordering::AcqRel)
  }

  /// Drop every cached classification (invalidation flush). Capacity
  /// goes with the entries: a flushed cache must not pin a build
  /// storm's bucket count.
  pub(crate) fn flush_proc_cache(&self) {
    let mut cache = self.proc_cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.clear();
    cache.shrink_to(0);
  }

  /// Record one classification (EXEC fast path, work re-reads).
  pub(crate) fn cache_proc(&self, pid: u64, epoch: u64, hit: Option<ScannedHit>, obs: bool) {
    self
      .proc_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .insert(pid, CachedProc { epoch, hit, obs });
  }

  /// Forget one pid (EXIT fast path: dead is dead, tracked or not).
  pub(crate) fn uncache_proc(&self, pid: u64) {
    self
      .proc_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .remove(&pid);
  }
}

#[cfg(test)]
#[path = "../../tests/unit/cache/procs.rs"]
mod tests;
