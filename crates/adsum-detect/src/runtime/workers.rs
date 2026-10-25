//! Worker plumbing: supervised spawns, the shutdown flag and the wake
//! registry every thread shares. Extracted verbatim from the old
//! `runtime.rs` top half — the rules (register once at startup, observe
//! the flag on every iteration and after every blocking wait) are the
//! contract `ProcessServer::shutdown` + `join` rely on.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Re-entrancy guard for `ProcessServer::scan_for_processes` (see
/// `crate::sweep`): acquired
/// atomically, released on drop (all exit paths, panics included).
pub struct ScanGuard {
  flag: Arc<AtomicBool>,
}

impl ScanGuard {
  /// Acquire the guard, or `None` when a scan is already in progress.
  pub fn try_acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
    flag
      .compare_exchange(
        false,
        true,
        std::sync::atomic::Ordering::Acquire,
        std::sync::atomic::Ordering::Relaxed,
      )
      .ok()?;
    Some(Self {
      flag: Arc::clone(flag),
    })
  }
}

impl Drop for ScanGuard {
  /// Release the re-entrancy flag (all exit paths, panics included).
  fn drop(&mut self) {
    self.flag.store(false, std::sync::atomic::Ordering::Release);
  }
}

/// Sleep the scan cadence. `park_timeout` (not `sleep`) so the proc-events
/// watcher can wake the loop early when a tracked game exits; a permit
/// stored by an unpark during scan work just causes one early rescan,
/// which downstream dedups harmlessly.
pub(crate) fn wait_scan(wait_time: Duration) {
  std::thread::park_timeout(wait_time);
}

/// Whether directed shutdown was requested (checked on every loop
/// iteration and after every blocking wait).
pub(crate) fn is_shutting_down(shutdown: &Arc<AtomicBool>) -> bool {
  shutdown.load(std::sync::atomic::Ordering::Acquire)
}

/// Register the calling thread for shutdown unpark. Called once per
/// spawned thread at startup (not per wait: the registry stays one
/// entry per thread for the server lifetime).
pub(crate) fn track_shutdown_thread(wake: &Arc<Mutex<Vec<std::thread::Thread>>>) {
  wake
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .push(std::thread::current());
}

/// Spawn a supervised worker: the handle joins `worker_handles` for
/// `join`, so no thread outlives a joined server. Takes the registry by
/// value (callers pass `Arc::clone`) so the moved body closure never
/// fights a borrow of its owner. Threads carry `adsum-*` names so
/// teardown accounting (and `top`/`ps`) can observe them.
pub(crate) fn spawn_worker(
  handles: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
  name: &str,
  body: impl FnOnce() + Send + 'static,
) {
  let handle = std::thread::Builder::new()
    .name(name.to_string())
    // Same failure semantics as `thread::spawn` (panics when the OS
    // refuses a thread): startup cannot proceed half-supervised.
    .spawn(body)
    .expect("scanner worker spawn failed");
  handles
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .push(handle);
}

/// Wake the scan thread if registered (best-effort): used when the
/// watcher fails so one fresh poll compensates immediately instead of
/// sleeping into the failure with possibly stale state. Missing handle:
/// silent no-op. Linux-only caller (watcher retry thread); allowed dead
/// elsewhere like [`ProcessServer::should_wake_on_exit`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn unpark_scan_wake(scan_wake: &Arc<Mutex<Option<std::thread::Thread>>>) {
  if let Some(thread) = scan_wake.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
    thread.unpark();
  }
}
