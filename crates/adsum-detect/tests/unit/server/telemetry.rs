use super::*;

/// N23: the gauge handed out for the census is the watcher's own gauge,
/// not a snapshot of it.
///
/// A copy would report whatever the counter read at handover time and
/// then freeze, so a shed after the census wiring would stay invisible —
/// exactly the blind spot N23 is about. The handle is refcounted, so
/// reads and writes land on the same counters.
#[test]
fn watch_gauge_handle_shares_the_watcher_counters() {
  let server = fixture_server();
  // No watcher on this fixture (and none in unit tests generally), so
  // install the gauge the way `start` does and prove the handle still
  // tracks the shared counters afterwards.
  let (tx, rx) = adsum_telemetry::QueueGauge::pair::<u32>();
  let real = tx.gauge();
  *server
    .watch_gauge
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(real.clone());

  let handle = server.watch_gauge().expect("gauge is installed");
  assert_eq!(
    handle.dropped_total(),
    0,
    "a fresh watch queue has shed nothing"
  );

  // Shed past the cap on the real channel end: the counter the census
  // reads must move with it.
  let cap = 2;
  for _ in 0..(cap + 1) {
    let _ = tx.send_checked(1, cap);
  }
  assert!(
    rx.recv().is_ok(),
    "the queued item must arrive for the gauge to settle"
  );
  let shed = handle.dropped_total();
  assert_eq!(
    shed,
    real.dropped_total(),
    "the census handle must read the watcher's own counter"
  );
  assert!(shed > 0, "the reference end must have shed past its cap");
}
