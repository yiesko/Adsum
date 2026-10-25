use super::*;
use adsum_types::DetectSource;

/// A delivery the channel accepts must read as complete, so the caller
/// may record the new table.
#[test]
fn accepted_events_report_a_complete_delivery() {
  let (tx, _rx) = adsum_telemetry::QueueGauge::pair::<ProcessDetectedEvent>();
  assert_eq!(
    deliver_snapshot(&tx, &[], &[placeholder_hit()], true),
    SnapshotDelivery::Complete,
    "an empty channel accepts the tick's events"
  );
}
/// The regression this guards: when the channel sheds, the delivery must
/// NOT read as complete.
///
/// The old order recorded the new table *before* sending, so a lost
/// event was never re-sent — the consumer kept a table it had already
/// been told to forget, and the next tick's delta was computed against
/// the new one. That is the ghost clear (and the ghost card).
#[test]
fn a_shed_event_never_reports_complete() {
  let (tx, rx) = adsum_telemetry::QueueGauge::pair::<ProcessDetectedEvent>();
  // Saturate the backlog with real events, no receiver draining.
  let cap = SNAPSHOT_EVENT_CAP;
  while matches!(
    tx.send_checked(ProcessDetectedEvent::Cleared, cap),
    SendChecked::Sent
  ) {}
  let hit = placeholder_hit();
  let delivered = deliver_snapshot(&tx, &[], &[hit], true);
  assert_eq!(
    delivered,
    SnapshotDelivery::Shed,
    "a shed event must be reported, never treated as delivered"
  );
  assert!(
    matches!(delivered, SnapshotDelivery::Shed),
    "redundant but explicit: only a shed delivery keeps the previous table"
  );
  drop(rx);
}

/// A gone receiver is shutdown, not a shed: the caller must end the
/// thread rather than re-emit forever.
#[test]
fn a_gone_receiver_reports_closed() {
  let (tx, rx) = adsum_telemetry::QueueGauge::pair::<ProcessDetectedEvent>();
  drop(rx);
  assert_eq!(
    deliver_snapshot(&tx, &[], &[placeholder_hit()], true),
    SnapshotDelivery::Closed,
    "a gone receiver is shutdown, not a shed"
  );
}

/// The ordering the whole fix is about: a shed delivery must leave the
/// recorded table untouched.
///
/// The old code assigned `last_emitted` *before* sending, so a lost
/// event was never re-sent and the consumer was left with a table it
/// had already been told to forget. This test drives the loop's own
/// advance step with an injected delivery, so it fails on that order —
/// the earlier tests only covered `deliver_snapshot`, and the injection
/// slipped straight past them.
#[test]
fn a_shed_tick_keeps_the_recorded_table() {
  let mut recorded: Vec<(AppId, u64)> = vec![("a".into(), 1)];
  let mut snapshot: Vec<(AppId, u64)> = vec![("b".into(), 2)];

  let delivery = advance_snapshot_lane(&mut recorded, &mut snapshot, |removed| {
    assert_eq!(
      removed,
      &[(AppId::from("a"), 1)],
      "delta from the recorded table"
    );
    SnapshotDelivery::Shed
  });

  assert_eq!(delivery, SnapshotDelivery::Shed);
  assert_eq!(
    recorded,
    vec![(AppId::from("a"), 1)],
    "a shed tick must not advance the recorded table"
  );
  assert_eq!(
    snapshot,
    vec![(AppId::from("b"), 2)],
    "a shed tick must keep the undelivered table in the scratch buffer"
  );
}

/// The mirror image: a complete delivery records the new table, so the
/// next tick's delta starts from what the consumer actually holds.
#[test]
fn a_complete_tick_records_the_new_table() {
  let mut recorded: Vec<(AppId, u64)> = vec![("a".into(), 1)];
  let mut snapshot: Vec<(AppId, u64)> = vec![("b".into(), 2)];

  let delivery =
    advance_snapshot_lane(&mut recorded, &mut snapshot, |_| SnapshotDelivery::Complete);

  assert_eq!(delivery, SnapshotDelivery::Complete);
  assert_eq!(
    recorded,
    vec![(AppId::from("b"), 2)],
    "a delivered tick must be recorded"
  );
  // The swap handed the old table back to the scratch buffer: the loop
  // clears it on the next build, so the reuse allocates nothing.
  assert_eq!(snapshot, vec![(AppId::from("a"), 1)]);
}

/// A shed delivery must leave the delta reproducible: with the previous
/// table still in place, the next tick computes the same removals
/// again. That is what makes "re-emit next tick" a repair instead of a
/// duplicate storm.
#[test]
fn a_shed_tick_recomputes_the_same_delta() {
  let previous = vec![(AppId::from("a"), 1u64), (AppId::from("b"), 2)];
  let current = vec![(AppId::from("a"), 1u64)];
  let mut first = removed_slots(&previous, &current);
  let mut second = removed_slots(&previous, &current);
  first.sort();
  second.sort();
  assert_eq!(
    first, second,
    "the delta must be reproducible when re-emitted"
  );
}

/// One placeholder hit for the delivery tests (pid/entry are irrelevant).
fn placeholder_hit() -> ScannedHit {
  ScannedHit {
    entry: Arc::new(ScannedEntry {
      id: "1".into(),
      name: "G".into(),
      executables: Box::new([]),
      steam_ids: Box::new([]),
      epic_ids: Box::new([]),
      aliases: Box::new([]),
    }),
    pid: 1,
    start: 0,
    source: DetectSource::Automaton,
  }
}
