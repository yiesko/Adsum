//! Process sweeping: the tick orchestration — enumerate processes, run
//! the full or the incremental mode over them, wake early on tracked
//! exits.
//!
//! This module is 100% platform-neutral: the OS-specific work sits
//! behind the `ProcessEnumerator` seam (`crate::platform::PlatformEnumerator`
//! — raw `/proc` + `getdents64` on Linux, the shared `sysinfo` snapshot
//! everywhere else), and both tick modes — the full sweep and the
//! incremental cache — orchestrate through it without a single cfg
//! branch here. Classification funnels through [`matching`] and the
//! shared bundle in [`generations`](super::generations); ticks stay
//! allocation-free via the caller-owned [`scratch`] buffers.

use super::runtime::ScanGuard;
use super::server::ProcessServer;
use super::types::{Exec, ProcessScanState, ScannedHit};
use crate::platform::PlatformEnumerator;
// All four scratch groups exist on every target: the snapshot backend
// reuses the same enumeration state (pid list, liveness set, work lists)
// and compaction counter the /proc sweep does; only the Linux-only read
// buffers (cmdline/environ/dents) stay gated per field.
use crate::scratch::{EnumerateScratch, ExecScratch, ScanScratch};
use crate::traits::ProcessEnumerator;

// The Linux-only test seam (`process_list_into`) drives the procfs
// reader directly, exactly like the enumerator backend does.
#[cfg(all(target_os = "linux", test))]
use crate::platform::read_exec_into;

impl ProcessServer {
  /// Whether a tracked game's EXIT should wake the scan loop early.
  /// Untracked exits never wake (one HashSet lookup, consumed either
  /// way — dead is dead). Tracked ones wake at most once per second:
  /// a recent EXIT stays tracked so a follow-up EXIT can still wake —
  /// only an actual wake consumes the pid. Proton helpers die
  /// constantly, and every one of them matches the game, so unwedged
  /// wakes would unpark the loop several times per second (measured
  /// 0.76s effective cadence instead of 5s during NFS).
  #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
  pub fn should_wake_on_exit(&self, pid: u64) -> bool {
    // One guard for both the membership check and the removal: the pid is
    // consumed by an actual wake, so a follow-up EXIT of the same game
    // must still be able to wake — and taking the lock twice re-opened
    // a window where another EXIT could remove it in between, silently
    // dropping that wake.
    let mut pids = self.detected_pids.lock().unwrap_or_else(|e| e.into_inner());
    if !pids.contains(&pid) {
      return false;
    }
    let due = self
      .last_scan
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .elapsed()
      .as_secs()
      >= 1;
    if due {
      pids.remove(&pid);
    }
    due
  }

  /// Enumerate processes via `/proc` into caller-owned buffers: the
  /// combined test seam (the slot-reuse and compaction regression tests
  /// drive the Linux sweep as one call). Production ticks go through
  /// [`scan_for_processes`](ProcessServer::scan_for_processes), which
  /// runs the same two halves — `begin_tick` (getdents64) and
  /// [`fill_slots`](ProcessServer::fill_slots) — with the incremental
  /// mode between them.
  ///
  /// Unreadable pids (kernel threads, zombies, races) are skipped
  /// without leaving holes: only successful reads advance. Returns the
  /// filled prefix length; surplus high-water slots past it are kept
  /// (not truncated) so their string buffers survive count dips.
  /// Callers must use only `&processes[..filled]`.
  #[cfg(all(target_os = "linux", test))]
  pub(crate) fn process_list_into(
    processes: &mut Vec<Exec>,
    scratch: &mut ScanScratch,
  ) -> adsum_protocol::error::Result<usize> {
    crate::platform::linux::enumerator::enumerate_pids_into(&mut scratch.enumerate)?;
    let pids = std::mem::take(&mut scratch.enumerate.pids);
    let mut filled = 0usize;
    for &pid in pids.as_slice() {
      if filled == processes.len() {
        processes.push(Exec::default());
      }
      if read_exec_into(pid, &mut processes[filled], &mut scratch.exec_scratch) {
        filled += 1;
      }
    }
    scratch.enumerate.pids = pids;
    Self::compact_slots(processes, filled, &mut scratch.exec_scratch);
    Ok(filled)
  }

  /// Refill the `Exec` slots from the scratch's enumerated pid list
  /// through the platform reader: the `Vec` backing and every slot
  /// survive across ticks (in-place refill, never rebuilt), so a warm
  /// fill allocates nothing per process on ANY target — Linux reads
  /// `/proc/<pid>/cmdline` per slot, the snapshot backend copies out of
  /// the refreshed `sysinfo` table. Unreadable pids are skipped without
  /// leaving holes; returns the filled prefix length (callers use only
  /// `&processes[..filled]`).
  fn fill_slots(
    &self,
    processes: &mut Vec<Exec>,
    enumerate: &mut EnumerateScratch,
    exec: &mut ExecScratch,
  ) -> usize {
    // Lift the pid list out for the fill loop: the loop hands `scratch`
    // mutably to the per-pid reader, which the shared borrow of `pids`
    // would otherwise fight. `mem::take` moves the Vec (no allocation);
    // it is put back below so the next tick reuses the same backing.
    let pids = std::mem::take(&mut enumerate.pids);

    // No `clear` and no `truncate`: the Vec length is the high-water
    // slot count. Slots refill in place below, so a warm tick reuses
    // every string buffer; `push` only grows past the high-water mark.
    // Tail slots past `filled` keep their buffers for the next rise.
    let mut filled = 0usize;
    for &pid in pids.as_slice() {
      if filled == processes.len() {
        processes.push(Exec::default());
      }
      // `filled <= len` always: slots refill in place, `push` only grows
      // past the high-water mark.
      if PlatformEnumerator::read_one(self, pid, &mut processes[filled], exec) {
        filled += 1;
      }
    }
    enumerate.pids = pids;
    Self::compact_slots(processes, filled, exec);
    filled
  }

  /// Adaptive release of surplus slots, shared by every fill path: a
  /// build storm's spike would otherwise leave the slot list (and every
  /// tail slot's string buffers) at high-water for the rest of the
  /// session — RSS stable in content but pinned by one moment. After
  /// `SLOT_LOW_TICKS` consecutive ticks under half occupancy the
  /// surplus is released, so the next rise pays for it again. A single
  /// spike never reaches the threshold.
  fn compact_slots(processes: &mut Vec<Exec>, filled: usize, scratch: &mut ExecScratch) {
    if filled * 2 < processes.len() {
      scratch.low_ticks = scratch.low_ticks.saturating_add(1);
      if scratch.low_ticks >= crate::scratch::SLOT_LOW_TICKS {
        processes.truncate(filled);
        processes.shrink_to(filled);
        scratch.low_ticks = 0;
      }
    } else {
      scratch.low_ticks = 0;
    }
  }

  #[hotpath::measure]
  /// One process sweep, classifying every live process against the
  /// current generation bundle. `processes` + `scratch` are caller-owned
  /// reuse buffers (the scan loop keeps them across ticks; one-shot
  /// callers pass throwaways): a warm tick allocates nothing per process.
  ///
  /// Two modes, decided per tick on every platform:
  /// - **Incremental** (`cache_mode_live`: proc-events watcher enabled
  ///   AND live — today only possible on Linux, whose netlink watcher
  ///   is the invalidation source): enumerate for liveness, sweep the
  ///   dead, revalidate cached hits, and read + classify only the pids
  ///   the process cache (`cache::procs`) has no current-generation
  ///   entry for. Steady state: zero cmdline reads, zero automaton
  ///   walks, zero allocations per tick.
  /// - **Full** (watcher off/dead/retrying, one-shot diagnostics, and
  ///   every target without a watcher): the classic sweep — every pid's
  ///   slot is filled and classified each tick. Behaviorally identical
  ///   to the pre-cache scanner.
  ///
  /// Both modes return the same shape: hits in enumeration order.
  pub fn scan_for_processes(
    &self,
    processes: &mut Vec<Exec>,
    scratch: &mut ScanScratch,
  ) -> adsum_protocol::error::Result<Vec<ScannedHit>> {
    // One liveness/enumeration pass through the platform backend: fills
    // `scratch.enumerate.pids` in enumeration order (the order the
    // detected table reports in). On Linux this is the only syscall
    // batch a quiet incremental tick makes at all.
    PlatformEnumerator::begin_tick(self, &mut scratch.enumerate)?;

    tracing::debug!("[Process Scanner] Process scan triggered");

    // Re-entrancy guard: a manual `scan_for_processes` racing the scan
    // thread (or two manual triggers) must not interleave. RAII so every
    // exit path — including `?` and panics — releases it.
    let _scan_guard = ScanGuard::try_acquire(&self.scanning).ok_or_else(|| {
      tracing::debug!("[Process Scanner] Scanning already in progress");
      adsum_protocol::error::AdsumError::ScanInProgress
    })?;

    let mut obs_open = false;

    // One generation for the whole tick: clone the Arc once, classify
    // every process against it. A refresh landing mid-tick only swaps in
    // the next bundle, which this tick simply won't see — no torn reads.
    let bundle = self.detectables.load_full();

    // One exclusions guard for the whole tick, hoisted out of the
    // per-process match (the set is rewritten hourly at most; a writer
    // landing mid-tick waits out the tick, which is milliseconds).
    let excl = self.exclusions_read();

    // Steam generation marker: one stat per watched libraryfolders.vdf;
    // the parse itself runs only when something actually changed.
    self.refresh_steam_libraries();
    // Heroic's cached Legendary library: same shape; one stat per tick once
    // the path is known (cheap discovery probes while absent), parsed only
    // when it changes (feeds the `-epicapp` fallback).
    self.refresh_heroic_apps();

    let detected_list: Vec<ScannedHit> = if self.cache_mode_live() {
      // The liveness set (scratch reuse buffer) feeds both sweeps: the
      // AppId memo's dead-pid drop and the process cache's dead-entry
      // retain. Built from the enumerated pids, never from the cache.
      {
        let live = &mut scratch.enumerate.live_pids;
        live.clear();
        live.extend(scratch.enumerate.pids.iter().copied());
      }
      // Watcher loss signal (sequence gap, backlog shed, socket overrun,
      // death or resubscribe): flush before trusting anything, so this
      // tick degrades to one full-cost re-read through the incremental
      // machinery — self-healing, never wrong.
      if self.take_cache_dirty() {
        self.flush_proc_cache();
      }
      // Drop memoized AppIds of dead pids (pid reuse must never serve a
      // stale id), against the same enumerated set.
      self.sweep_dead_appids_by_live(&scratch.enumerate.live_pids);

      self.scan_incremental(processes, &bundle, &excl, scratch, &mut obs_open)
    } else {
      // Classic full sweep: fill every slot from the pid list, sweep the
      // AppId memo against the FILLED table (unreadable pids drop their
      // memos at once, the historical rule), and classify each process.
      // The probe buffers live inside `scratch`, reused for every
      // process (see path_variants_into).
      let filled = self.fill_slots(processes, &mut scratch.enumerate, &mut scratch.exec_scratch);
      self.sweep_dead_appids(&processes[..filled], &mut scratch.enumerate.live_pids);
      processes[..filled]
        .iter()
        .filter_map(|process| self.match_process(process, &bundle, &excl, &mut obs_open, scratch))
        .collect()
    };

    let callback = self
      .event_listeners
      .lock()
      .map_err(|e| adsum_protocol::error::AdsumError::Poisoned("event_listeners", e.to_string()))?
      .on_process_scan_complete
      .clone();

    if let Some(callback) = callback.as_ref() {
      callback.lock().map_err(|e| {
        adsum_protocol::error::AdsumError::Poisoned("process callback", e.to_string())
      })?(ProcessScanState { obs_open });
    }

    // No `shrink_to_fit` here: the list is transient (built and consumed
    // within the tick), so forcing a realloc each scan only churns the
    // allocator. Long-lived tables shrink in their own place (see the
    // bundle build, which owns its tables for hours).
    tracing::debug!("[Process Scanner] Process scan complete");

    Ok(detected_list)
  }

  /// One incremental tick: sweep dead entries, re-read + reclassify only
  /// the missing/foreign-epoch pids, and build the tick's table from the
  /// cache in enumeration order. See the `cache::procs` module docs
  /// module docs for the trust model — every loss channel (gap, shed,
  /// overrun, watcher restart, bundle swap) degrades to at most one
  /// full-cost tick, never to a stale classification surviving one.
  fn scan_incremental(
    &self,
    processes: &mut Vec<Exec>,
    bundle: &crate::bundle::DetectablesBundle,
    excl: &crate::db::Exclusions,
    scratch: &mut ScanScratch,
    obs_open: &mut bool,
  ) -> Vec<ScannedHit> {
    let epoch = bundle.epoch;

    // 1) Sweep + work list under one lock hold: dead entries out (the
    //    enumerated set is the liveness truth), and every pid whose entry
    //    is missing or was classified against a foreign generation joins
    //    the work list. Cached HITS also join the revalidation list (a
    //    zombie is still enumerated but reads empty). A bundle swap
    //    therefore costs exactly one full-read tick — no writer ever
    //    touches the cache.
    {
      let mut cache = self.proc_cache.lock().unwrap_or_else(|e| e.into_inner());
      cache.retain(|pid, _| scratch.enumerate.live_pids.contains(pid));
      let (len, capacity) = (cache.len(), cache.capacity());
      let work = &mut scratch.enumerate.work_pids;
      let hits = &mut scratch.enumerate.hit_pids;
      work.clear();
      hits.clear();
      for &pid in scratch.enumerate.pids.as_slice() {
        match cache.get(&pid) {
          Some(entry) if entry.epoch == epoch => {
            // Current-generation entry: no work. Cached HITS join the
            // revalidation list (zombie guard, step 1.5); misses rest.
            if entry.hit.is_some() {
              hits.push(pid);
            }
          }
          _ => work.push(pid),
        }
      }
      drop(cache);
      // Same collapse-trim rule as the AppId memo sweep: a build storm's
      // bucket count must not be pinned for the rest of the session.
      if capacity > 64 && len * 4 < capacity {
        let target = (len * 2).max(16);
        let mut cache = self.proc_cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() * 4 < cache.capacity() {
          cache.shrink_to(target);
        }
      }
    }

    // 1.5) Revalidate cached hits: one read per detected game (0-3 per
    //    tick) through the platform reader, so a process that zombied
    //    between ticks cannot hold a stale card — zombies are still
    //    enumerated (the /proc dir outlives the address space; sysinfo
    //    lists them too) but read empty, and "unreadable = absent" is
    //    the rule the full sweep has always applied. Misses are not
    //    revalidated: a cached miss can only become a hit through an
    //    exec, and every exec either arrives as an EXEC event (replacing
    //    the entry) or was lost with a signal that flushed the whole
    //    cache. The read refills slot 0 in place; the entry itself is
    //    untouched when the pid is still readable, so a cached hit keeps
    //    its first-classification stamp.
    {
      let hits = std::mem::take(&mut scratch.enumerate.hit_pids);
      if !hits.is_empty() {
        if processes.is_empty() {
          processes.push(Exec::default());
        }
        for &pid in hits.as_slice() {
          if !PlatformEnumerator::read_one(self, pid, &mut processes[0], &mut scratch.exec_scratch)
          {
            self.uncache_proc(pid);
          }
        }
      }
      scratch.enumerate.hit_pids = hits;
    }

    // 2) Work pids: one read + one classification each, I/O and automaton
    //    walks OUTSIDE the cache lock (the dispatch thread's EXEC inserts
    //    contend for microseconds, never for a read). The slots are the
    //    caller's high-water Vec, refilled in place; a pid that vanished
    //    mid-read leaves no entry and simply is not in the table — the
    //    same skip the full sweep has always had.
    {
      let work = std::mem::take(&mut scratch.enumerate.work_pids);
      let mut filled = 0usize;
      for &pid in work.as_slice() {
        if filled == processes.len() {
          processes.push(Exec::default());
        }
        if PlatformEnumerator::read_one(
          self,
          pid,
          &mut processes[filled],
          &mut scratch.exec_scratch,
        ) {
          let mut ev_obs = false;
          let hit = self.match_process(&processes[filled], bundle, excl, &mut ev_obs, scratch);
          self.cache_proc(pid, epoch, hit, ev_obs);
          filled += 1;
        }
      }
      scratch.enumerate.work_pids = work;
    }

    // 3) The tick's table: cached classifications for the enumerated
    //    pids, in enumeration order (the full sweep's shape). Hits are
    //    Arc bumps off the shared entries; `obs_open` is the OR of the
    //    live entries' bits — what a full sweep would have computed from
    //    the same paths.
    let cache = self.proc_cache.lock().unwrap_or_else(|e| e.into_inner());
    let mut detected: Vec<ScannedHit> = Vec::new();
    for &pid in scratch.enumerate.pids.as_slice() {
      if let Some(entry) = cache.get(&pid) {
        *obs_open |= entry.obs;
        if let Some(hit) = &entry.hit {
          detected.push(hit.clone());
        }
      }
    }
    detected
  }
}
