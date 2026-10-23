use super::*;

/// Vanished slots (including pid rotations) report once; survivors never do.
#[test]
fn removed_slots_reports_only_vanished_ids() {
  let previous = vec![("a".to_string(), 1), ("b".to_string(), 2)];
  let current = vec![("b".to_string(), 2)];
  assert_eq!(
    removed_slots(&previous, &current),
    vec![("a".to_string(), 1)]
  );
  // Identical tables: nothing to clear (downstream dedups repeats).
  assert!(removed_slots(&current, &current).is_empty());
  // Empty current: full-table clear path handles it, not per-slot.
  // `removed_slots` still reports all previous as vanished; the caller
  // only uses it when `detected` is non-empty (see emission block).
  assert_eq!(removed_slots(&previous, &[]).len(), 2);
  // Pid replacement rotates the card: same id, new pid reports the old
  // pair as removed so the bridge clears before republishing.
  let restarted = vec![("a".to_string(), 9), ("b".to_string(), 2)];
  assert_eq!(
    removed_slots(&previous, &restarted),
    vec![("a".to_string(), 1)]
  );
}

/// Ignored app ids never publish on the EXEC fast path either.
#[test]
fn exec_path_skips_ignored_ids() {
  use std::collections::HashSet;
  let hit = ScannedHit::stamp(
    std::sync::Arc::new(ScannedEntry::from_activity(&custom_entry())),
    4242,
    DetectSource::Automaton,
  );
  let ignored: HashSet<String> = ["777".to_string()].into_iter().collect();
  assert!(exec_hit_ignored(&ignored, &hit));
  let empty: HashSet<String> = HashSet::new();
  assert!(!exec_hit_ignored(&empty, &hit));
}

/// Backoff stretches idle ticks only while the watcher is confirmed live.
#[test]
fn backoff_applies_only_with_live_watcher() {
  use std::time::Duration;
  let base = Duration::from_secs(5);
  // Polling-only paths (other OSes, disabled/failed watcher) never back
  // off: a short configured interval stays short.
  assert_eq!(scan_cadence(base, 4, false), base);
  assert_eq!(scan_cadence(base, 0, false), base);
  // Live watcher: idle ticks stretch up to the 30s cap.
  assert_eq!(scan_cadence(base, 0, true), base);
  assert_eq!(scan_cadence(base, 4, true), Duration::from_secs(30));
}

/// The debounce consumes the pid: a second EXIT for the same game
/// inside the 1s window must not wake again.
///
/// This is the invariant the single-lock version of
/// `should_wake_on_exit` exists to keep — taking the lock twice for the
/// membership check and the removal re-opened the window where two EXITs
/// of the same pid both pass the check, wake twice, and only then consume
/// once. Proton helpers die constantly, so a duplicated wake is not a
/// rare corner.
#[test]
fn a_consumed_exit_pid_does_not_wake_twice() {
  let server = fixture_server();
  let pid = std::process::id().into();
  server
    .detected_pids
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .insert(pid);
  // Steal the last_scan latch so the debounce is "due" immediately.
  *server.last_scan.lock().unwrap_or_else(|e| e.into_inner()) =
    std::time::Instant::now() - std::time::Duration::from_secs(2);

  assert!(server.should_wake_on_exit(pid), "the first exit wakes");
  assert!(
    !server.should_wake_on_exit(pid),
    "the pid was consumed, so a second exit must not wake"
  );
  assert!(
    server
      .detected_pids
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .is_empty(),
    "the consumed pid must not be remembered for the next sweep"
  );
}

/// An EXIT that arrives before the debounce is due does not consume the
/// pid: a follow-up EXIT still has its chance to wake.
#[test]
fn an_early_exit_keeps_the_pid_for_a_later_one() {
  let server = fixture_server();
  let pid = std::process::id().into();
  server
    .detected_pids
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .insert(pid);

  // last_scan is fresh, so the debounce is not due.
  assert!(
    !server.should_wake_on_exit(pid),
    "inside the debounce window there is no wake"
  );
  assert!(
    server
      .detected_pids
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .contains(&pid),
    "an early exit must leave the pid tracked"
  );
}
