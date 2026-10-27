//! The netlink watcher wiring: EXEC/EXIT dispatch and the supervised
//! `watch()` retry loop, spawned from `ProcessServer::start` on Linux.
//!
//! The dispatch thread is the fast path's brain: it reads one pid's
//! cmdline into a reused slot, classifies it through the SAME shared
//! pipeline the tick uses (`ProcessServer::match_process`), feeds the
//! incremental process cache, and publishes hits at once. EXITs of
//! tracked games wake the scan loop (debounced in `should_wake_on_exit`).

use std::sync::Arc;
use std::time::Duration;

use crate::cache::procs::argv_fingerprint;
use crate::runtime::lanes::{EXEC_EVENT_CAP, exec_hit_ignored, publish_exec_hit};
use crate::runtime::workers::{
  is_shutting_down, spawn_worker, track_shutdown_thread, unpark_scan_wake,
};
use crate::scratch::ScanScratch;
use crate::server::ProcessServer;
use crate::types::Exec;

use super::proc_readers::read_exec_into;

/// Cadence between dispatch receives: the only blocking wait in the
/// dispatch loop doubles as the shutdown poll. Traffic returns at once;
/// silence costs one cheap timeout per interval.
const DISPATCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Spawn the netlink watcher + dispatch threads. Returns the watcher's
/// backlog gauge so the caller can store it where the census reads depth
/// and shed totals (see `ProcessServer::watch_gauge`).
pub(crate) fn spawn_proc_watcher(server: &ProcessServer) -> adsum_telemetry::QueueGauge {
  use adsum_proc_events::{ProcEvent, in_initial_user_namespace, watch};
  use adsum_telemetry::QueueGauge;

  let (tx, rx) = QueueGauge::pair();
  // Read the gauge before the sender moves into the watcher thread: it is
  // a refcounted handle, so this clone still sees every shed.
  let gauge = tx.gauge();
  let dispatch = server.clone();
  spawn_worker(
    Arc::clone(&server.worker_handles),
    "adsum-dispatch",
    move || {
      // EXEC reuse buffers, owned by this loop: one `Exec` slot plus the
      // read scratch, refilled per event instead of allocated per event.
      // The classification tail reuses the same grouped scratch (the
      // probe buffers live inside it).
      let mut exec_slot = Exec::default();
      let mut scratch = ScanScratch::default();
      loop {
        // Exit at once when shutdown landed during the previous message
        // handling: the bounded wait below would otherwise park up to a
        // second needlessly.
        if is_shutting_down(&dispatch.shutdown) {
          break;
        }
        // Bounded wait: every expiry re-checks directed shutdown so the
        // thread never blocks past it; a gone sender still ends the loop
        // at once, exactly like the `while let` before.
        let event = match rx.recv_timeout(DISPATCH_POLL_INTERVAL) {
          Ok(event) => event,
          Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            if is_shutting_down(&dispatch.shutdown) {
              break;
            }
            continue;
          }
          Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match event {
          ProcEvent::Exec(pid) => {
            if !read_exec_into(pid, &mut exec_slot, &mut scratch.exec_scratch) {
              tracing::debug!("[Process Scanner] exec event: pid {pid} unreadable, skipping");
              continue;
            }
            let exec = &exec_slot;
            // Same pid, new image: the memoized AppId may be stale.
            dispatch.drop_appid(pid);
            // One generation for the whole classification: a refresh
            // landing mid-probe can only swap in the next bundle, which
            // this event simply won't see.
            let bundle = dispatch.bundle();
            let mut obs_open = false;
            // One exclusions guard per event: EXECs are rare next to the
            // poll they backstop, and the guard mirrors the scan tick's
            // hoisted read (same data, same fail-open recovery).
            let excl = dispatch.exclusions_read();
            let matched = dispatch.match_process(exec, &bundle, &excl, &mut obs_open, &mut scratch);
            // Feed the incremental cache BEFORE publishing: the next poll
            // tick finds this pid classified against this generation and
            // skips it entirely. The epoch travels inside the bundle Arc,
            // so the entry can never claim a generation it was not
            // classified against. A miss is cached too — `None` is a
            // verified miss, and re-walking the automata for the same
            // unchanged pid every tick is exactly the redundancy the
            // cache exists to remove.
            dispatch.cache_proc(
              pid,
              bundle.epoch,
              argv_fingerprint(exec),
              matched.clone(),
              obs_open,
            );
            if let Some(hit) = matched {
              // Coexistence parity with the polling path: ignored IDs never
              // publish, even on the event-driven fast path.
              if exec_hit_ignored(&dispatch.ignored_ids, &hit) {
                tracing::debug!(
                  "[Process Scanner] exec event: pid {pid} ignored ({}), skipping",
                  hit.entry.id.as_ref() as &str
                );
                continue;
              }
              let game_pid = hit.pid;
              tracing::debug!(
                "[Process Scanner] exec event: pid {pid} matched {}",
                hit.entry.name
              );
              dispatch
                .detected_pids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(game_pid);
              // Mark the scan dirty: this publication bypasses the polling
              // snapshot, so the next tick must emit its full table even if
              // unchanged — otherwise a game that dies before any poll
              // observes it would never emit its clear (see `scan_dirty`).
              dispatch
                .scan_dirty
                .store(true, std::sync::atomic::Ordering::Release);
              // Best-effort publication: a shed backstops through
              // `scan_dirty` (set above), which forces the next tick to
              // re-emit the whole table. Only a gone receiver ends the
              // thread.
              if !publish_exec_hit(&dispatch.event_sender, hit, EXEC_EVENT_CAP) {
                break;
              }
            }
          }
          ProcEvent::Exit(pid) => {
            // Dead is dead, tracked or not: the cache entry goes at once
            // (the tick's sweep would drop it anyway; removing here keeps
            // a pid reuse between ticks from ever meeting a stale entry).
            dispatch.uncache_proc(pid);
            // Only tracked games MAY wake the scan — decided in one place
            // so the debounce is unit-testable (see below).
            if dispatch.should_wake_on_exit(pid)
              && let Some(thread) = dispatch
                .scan_wake
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
              thread.unpark();
            }
          }
        }
      }
    },
  );
  // Best-effort watcher with periodic resubscribe: a failed self-test
  // (or a mid-run socket death) falls back to polling, but a transient
  // kernel stall must not pin polling until the next daemon restart —
  // delivery has been observed to resume on its own. First failure
  // warns, later ones stay in debug so genuinely unsupported systems do
  // not log every 5 minutes forever (the sandbox case never reaches
  // this loop: it is diagnosed once, before it).
  let watcher_state = server.clone();
  spawn_worker(
    Arc::clone(&server.worker_handles),
    "adsum-watch",
    move || {
      // cn_proc delivers only from the initial user namespace: inside a
      // user namespace the kernel silently ignores the subscription,
      // so no probe could ever prove anything and the retry loop would
      // spin through failed self-tests forever. Detect the sandbox
      // once, warn, and leave the watcher off — polling alone drives
      // detection (nothing was ever believed, so there is no cache
      // edge to invalidate here either).
      if !in_initial_user_namespace() {
        tracing::warn!(
          "[Process Scanner] cn_proc unavailable: running in a non-initial user namespace (sandbox), polling only; watcher disabled"
        );
        return;
      }
      // Register once: `shutdown` unparks this thread out of the retry
      // park below, so teardown never waits out the backoff ceiling.
      track_shutdown_thread(&watcher_state.shutdown_wake);
      let mut attempts = 0u32;
      // Exponential retry backoff: a first failure is likely a
      // transient kernel stall worth retrying soon, while a persistent
      // one settles into the old five-minute cadence.
      let mut backoff = Duration::from_secs(5);
      loop {
        // Directed shutdown before blocking in `watch()`: with the flag
        // set the call below returns at once (preset-stop fast path).
        if is_shutting_down(&watcher_state.shutdown) {
          break;
        }
        // Invalidate on BOTH edges of the watch: events between the last
        // full-mode tick and the (re)subscribe were never seen, and the
        // death window's were never delivered — either way the cache must
        // not be trusted when the watcher comes back. `watch` itself sets
        // the same flag on sequence gaps, backlog sheds and socket
        // overruns (the loss channels it can observe from inside).
        watcher_state
          .cache_dirty
          .store(true, std::sync::atomic::Ordering::Release);
        // Liveness (the scan-cadence hint) is owned by `watch()` — the
        // worker stores nothing: it is claimed only after a proven
        // self-test and cleared on every exit path (see `scan_cadence`).
        match watch(
          &tx,
          &watcher_state.shutdown,
          &watcher_state.cache_dirty,
          &watcher_state.watcher_live,
        ) {
          // Receiver gone: daemon shutting down. `watch()` already
          // cleared `watcher_live` on the way out.
          Ok(()) => {
            break;
          }
          Err(err) => {
            // Poll once now: the watcher just died, so the scan loop must
            // not sit out its whole backoff on possibly stale state.
            unpark_scan_wake(&watcher_state.scan_wake);
            attempts = attempts.saturating_add(1);
            if attempts == 1 {
              tracing::warn!(
                "[Process Scanner] proc-events unavailable ({err}), polling only; retrying"
              );
            } else {
              tracing::debug!(
                "[Process Scanner] proc-events still unavailable ({err}), polling only"
              );
            }
            // Interruptible park (not `sleep`): the pre-park check exits
            // at once when shutdown is already requested, otherwise the
            // unpark bounds the current backoff and the loop-top check
            // exits. Doubling happens AFTER the park: the wait that just
            // ended is the one the previous failure paid for.
            if is_shutting_down(&watcher_state.shutdown) {
              break;
            }
            std::thread::park_timeout(backoff);
            backoff = backoff.saturating_mul(2).min(Duration::from_secs(300));
          }
        }
      }
    },
  );
  gauge
}
