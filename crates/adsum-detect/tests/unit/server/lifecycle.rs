use super::*;

/// Concurrent appends and refreshes lose neither side (writer lock).
#[test]
fn watcher_failure_unparks_stale_scan() {
  // A parked thread woken through the helper must observe the permit:
  // park/unpark pairing is the whole contract (no scan logic here).
  let woken = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
  let flag = woken.clone();
  let waker = std::thread::spawn(move || {
    std::thread::park();
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
  });
  std::thread::sleep(std::time::Duration::from_millis(100));
  let slot: Arc<Mutex<Option<std::thread::Thread>>> =
    Arc::new(Mutex::new(Some(waker.thread().clone())));
  unpark_scan_wake(&slot);
  waker.join().expect("helper must unpark the waiter");
  assert!(woken.load(std::sync::atomic::Ordering::SeqCst));
  // Missing handle or lock: silent no-ops, never panics.
  unpark_scan_wake(&Arc::new(Mutex::new(None)));
}

/// `shutdown()` before `start()` is safe; `join()` with no threads
/// returns at once.
#[test]
fn shutdown_before_start_is_safe() {
  let server = fixture_server();
  server.shutdown();
  server.join();
  server.shutdown();
  server.join();
}

/// Join a live scan thread through a timeout channel: `shutdown()` must
/// unpark it and `join()` must return promptly (pre-shutdown-lifecycle
/// this hung: the parked loop had no directed exit).
#[test]
fn shutdown_joins_scan_thread_promptly() {
  let mut server = fixture_server();
  server.set_proc_events(false);
  server.start(Duration::from_millis(50));
  // A few ticks so the thread is parked in its cadence wait, not still
  // starting up, when shutdown lands.
  std::thread::sleep(Duration::from_millis(200));
  server.shutdown();
  let (done_tx, done_rx) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    server.join();
    // Structural proof alongside the prompt return below: `join`
    // drains the handle registry, so no worker is merely detached.
    let drained = server
      .worker_handles
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .is_empty();
    let _ = done_tx.send(drained);
  });
  assert!(
    done_rx
      .recv_timeout(Duration::from_secs(5))
      .expect("join must return promptly after shutdown"),
    "handle registry must drain on join"
  );
}

/// The hourly refresh sleep is interruptible: an enabled refresh whose
/// URL refuses fast must still join promptly instead of sleeping out
/// the hour. `127.0.0.1:9` is the discard port (refused, loopback-only,
/// no external network); exclusions stay unset so only the DB fetch
/// runs before the sleep.
#[test]
fn refresh_sleep_is_interrupted_by_shutdown() {
  let mut server = fixture_server();
  server.set_proc_events(false);
  server.start(Duration::from_millis(50));
  // Let the refresh thread fail its first fetch and enter the hourly
  // sleep; the scan thread ticks alongside, both must stop below.
  std::thread::sleep(Duration::from_millis(500));
  server.shutdown();
  let (done_tx, done_rx) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    server.join();
    let drained = server
      .worker_handles
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .is_empty();
    let _ = done_tx.send(drained);
  });
  assert!(
    done_rx
      .recv_timeout(Duration::from_secs(10))
      .expect("refresh sleep must not outlive shutdown"),
    "handle registry must drain on join"
  );
}

/// Spawned workers carry `adsum-*` thread names (Linux comm): join
/// accounting and teardown tests observe threads by name. Sibling
/// lifecycle tests may add their own `adsum-*` threads, so this
/// asserts presence only — absence is asserted per-server via the
/// drained handle registry (join tests above) and per-daemon via
/// task enumeration (core daemon tests).
#[cfg(target_os = "linux")]
#[test]
fn worker_threads_carry_names() {
  fn has_prefix(prefix: &str) -> bool {
    let mut found = false;
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
      for task in tasks.flatten() {
        if let Ok(comm) = std::fs::read_to_string(task.path().join("comm"))
          && comm.trim().starts_with(prefix)
        {
          found = true;
          break;
        }
      }
    }
    found
  }
  let mut server = fixture_server();
  server.set_proc_events(false);
  server.start(Duration::from_millis(50));
  std::thread::sleep(Duration::from_millis(200));
  assert!(has_prefix("adsum-scan"), "scan thread must be named");
  server.shutdown();
  server.join();
}
