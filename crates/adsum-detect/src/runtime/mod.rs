//! Scanner thread lifecycle: supervised workers with directed shutdown.
//!
//! Every thread `start` spawns (scan loop, database refresh, proc-events
//! dispatch and watcher retry) registers here and observes the shared
//! shutdown flag, so `ProcessServer::shutdown` plus `join` ends them all
//! promptly instead of waiting out hourly sleeps. Workers carry `adsum-*`
//! names so teardown accounting (and `top`/`ps`) can observe them.
//!
//! # Layout
//!
//! This module holds the lifecycle itself (`start`, `shutdown`, `join`,
//! [`ShutdownHandle`]); the machinery lives beside it: `workers`
//! (supervised spawns, the flag and the wake registry), `lanes` (the
//! snapshot/EXEC event lanes and the result post-processors),
//! `cadence` (idle backoff and its watcher gate) and `loops` (the two
//! long-running thread bodies, extracted from the old inline closures).

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::server::ProcessServer;

pub(crate) mod cadence;
pub(crate) mod lanes;
pub(crate) mod loops;
pub(crate) mod workers;

// Historical surface: `server.rs` re-exports `ScanGuard`/`idle_wait` for
// 0.36-path compatibility, the daemon filters through `apply_ignore_list`
// and the test suites glob this module for everything else.
pub use cadence::idle_wait;
pub use lanes::{apply_ignore_list, first_sightings};
pub use workers::ScanGuard;

use loops::{run_refresh_loop, run_scan_loop};
use workers::{spawn_worker, unpark_scan_wake};

#[cfg(test)]
pub(crate) use lanes::*;

/// The sender wraps an unbounded `std::mpsc`, so a plain `send` only
/// fails when the receiver is gone — which is why the lane uses
/// [`publish_exec_hit`] and [`deliver_snapshot`], both of which report
/// that case as shutdown and every other outcome as delivered-or-shed.
#[cfg(test)]
#[path = "../../tests/unit/runtime/mod.rs"]
mod tests;

/// Shared shutdown signaling without ownership: drop-guards and other
/// non-owners stop the threads; joining stays with `join`. Holds no
/// channel sender, so dropping it never keeps the event pump alive.
#[derive(Clone)]
pub struct ShutdownHandle {
  shutdown: Arc<AtomicBool>,
  wake: Arc<Mutex<Vec<std::thread::Thread>>>,
  scan_wake: Arc<Mutex<Option<std::thread::Thread>>>,
}

impl ShutdownHandle {
  /// Signal directed shutdown: parked threads are unparked; every loop
  /// observes the flag on its next iteration. Idempotent.
  pub fn signal(&self) {
    self
      .shutdown
      .store(true, std::sync::atomic::Ordering::Release);
    for thread in self.wake.lock().unwrap_or_else(|e| e.into_inner()).iter() {
      thread.unpark();
    }
    unpark_scan_wake(&self.scan_wake);
  }
}

impl ProcessServer {
  /// A signaling-only view of this server's shutdown state, for owners
  /// that must stop threads without joining (async drop guards): holds
  /// no channel sender, so it can never keep the event pump alive.
  pub fn shutdown_handle(&self) -> ShutdownHandle {
    ShutdownHandle {
      shutdown: Arc::clone(&self.shutdown),
      wake: Arc::clone(&self.shutdown_wake),
      scan_wake: Arc::clone(&self.scan_wake),
    }
  }

  /// Signal directed shutdown: every thread spawned by `start`
  /// observes the flag on its next iteration and exits; parked threads
  /// are unparked so no sleep runs its full duration. Idempotent:
  /// repeated calls only re-unpark. Pair with `join` to wait out the
  /// exits. (The CLI relies on process exit instead and never calls
  /// either: unchanged behavior there.)
  ///
  /// # Startup race
  ///
  /// Every worker registers and checks the flag first thing (refresh
  /// exits before any fetch; scan and retry exit at their loop-top
  /// checks; dispatch exits at its first receive timeout), so even a
  /// shutdown that lands mid-startup parks nothing: the unpark permit
  /// only shortens waits already in flight.
  pub fn shutdown(&self) {
    self.shutdown_handle().signal();
  }

  /// Block until every thread spawned by `start` has exited. Call after
  /// `shutdown`: joining without it waits out sleeps (refresh hours,
  /// watcher retry minutes). Takes the handle registry, so a second
  /// `join` returns at once. A panicked worker logs and continues
  /// joining the rest instead of abandoning them.
  pub fn join(&self) {
    let handles: Vec<std::thread::JoinHandle<()>> = std::mem::take(
      &mut *self
        .worker_handles
        .lock()
        .unwrap_or_else(|e| e.into_inner()),
    );
    for handle in handles {
      if handle.join().is_err() {
        tracing::warn!("[Process Scanner] worker thread panicked during join");
      }
    }
  }
}

impl ProcessServer {
  /// Start scan/refresh threads.
  ///
  /// The watcher's gauge is stored on the server, where the census reads
  /// depth and shed totals through [`ProcessServer::watch_gauge`].
  pub fn start(&self, scan_interval: Duration) {
    // Double-start is a caller bug: ignore fail-safe instead of leaking
    // a second scan/dispatch/watch generation.
    if self.started.swap(true, std::sync::atomic::Ordering::AcqRel) {
      tracing::warn!("[Process Scanner] Already started, ignoring duplicate start");
      return;
    }
    let wait_time = scan_interval;
    let clone = self.clone();

    // No custom rebuild needed here: new() already built the bundle with
    // its (empty) custom side, and appends rebuild on their own path.

    // Periodically refresh the detectable games database (like pog5-adsum).
    // Sleep first: startup already fetched synchronously, so an immediate
    // refetch would parse the whole DB twice for the same data (double
    // transient memory + startup time for zero new information). Refreshes
    // are conditional (ETag): an unchanged database costs one header round
    // trip and zero parsing, so steady-state RSS never ratchets.
    if clone.refresh.enable && clone.refresh.db_url.is_some() {
      let db_clone = clone.clone();
      // Hoisted once: `db_url` is immutable after `new()`, so the hourly
      // thread never unwraps an `Option` per iteration.
      let db_url = db_clone
        .refresh
        .db_url
        .clone()
        .expect("[bug] db_url checked above");
      spawn_worker(
        Arc::clone(&db_clone.worker_handles),
        "adsum-refresh",
        move || run_refresh_loop(db_clone, db_url),
      );
    }

    spawn_worker(Arc::clone(&clone.worker_handles), "adsum-scan", move || {
      run_scan_loop(clone, wait_time)
    });

    // Event-driven fast path (Linux): EXEC classifies one process at once,
    // EXIT of a tracked game wakes the scan above. Best-effort — setup
    // failure keeps pure polling, silently. The watcher's gauge stays
    // inside: the census reads it directly, so nothing is plumbed up.
    #[cfg(target_os = "linux")]
    if self.enable_proc_events {
      // Stored, not returned: the census reads it straight off the server.
      let gauge = crate::platform::linux::proc_events::spawn_proc_watcher(self);
      *self
        .watch_gauge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gauge);
    } else {
      tracing::info!(
        "[Process Scanner] proc-events watcher disabled by configuration, polling only"
      );
    }
  }
}
