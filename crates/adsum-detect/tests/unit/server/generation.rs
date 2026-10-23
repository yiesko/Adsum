use super::*;

/// Clones share one generation pointer in both directions.
#[test]
fn clones_share_one_detection_generation() {
  // The scan loop runs on a clone made in `start()`, while the hourly
  // refresh swaps through its own clone: both must observe the same
  // generation pointer, or refreshed databases never reach the scan.
  let server = fixture_server();
  let scan_clone = server.clone();
  server.append_detectables(vec![custom_entry()]);
  assert_eq!(
    scan_clone.bundle().custom.len(),
    server.bundle().custom.len(),
    "clone must observe overrides appended after cloning"
  );
  assert_eq!(scan_clone.bundle().custom.len(), 1);

  // The reverse direction holds too: a swap through the clone is
  // visible to the original.
  let reverse = scan_clone.clone();
  scan_clone.append_detectables(vec![custom_entry()]);
  assert_eq!(reverse.bundle().custom.len(), server.bundle().custom.len());
  assert_eq!(reverse.bundle().custom.len(), 2);
}

/// Refused bundle swaps report failure so validators are never committed.
#[test]
fn refused_bundle_swap_reports_failure() {
  // Empty input and failing builds must report failure so the refresh
  // thread keeps its old validators (next hour retries the data instead
  // of trusting tags for a bundle that was never installed).
  let server = fixture_server();
  assert!(!server.update_main_detectables(vec![]));
  assert!(server.update_main_detectables(vec![custom_entry()]));
}

/// Concurrent appends and refreshes lose neither side (writer lock).
#[test]
fn concurrent_writers_preserve_both_sides() {
  // Refresh (`update_main_detectables`) and override rebuilds
  // (`rebuild_custom`) both load -> build -> store: with a shared
  // generation and no serialization, one store can silently discard
  // the other's input. Every append must survive the concurrent
  // refreshes (the writer lock serializes them).
  const THREADS: usize = 4;
  const PER_THREAD: usize = 8;
  const REFRESHES: usize = 20;
  let server = fixture_server();
  std::thread::scope(|scope| {
    for t in 0..THREADS {
      let writer = server.clone();
      scope.spawn(move || {
        for i in 0..PER_THREAD {
          writer.append_detectables(vec![custom_entry_named(&format!("{t}-{i}"))]);
          // Spread appends over the refresh window so the writers
          // actually overlap (a stress test, not a timing benchmark).
          std::thread::sleep(std::time::Duration::from_micros(250));
        }
      });
    }
    let refresher = server.clone();
    scope.spawn(move || {
      for _ in 0..REFRESHES {
        refresher.update_main_detectables(main_entries(300));
      }
    });
  });
  assert_eq!(
    server.bundle().custom.len(),
    THREADS * PER_THREAD,
    "no override may be lost to a concurrent refresh"
  );
  assert_eq!(server.bundle().list.len(), 300, "refresh result must stick");
}
