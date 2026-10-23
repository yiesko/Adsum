use super::*;

/// A hit the channel accepts is published.
#[test]
fn an_accepted_exec_hit_is_published() {
  let (tx, _rx) = adsum_telemetry::QueueGauge::pair::<ProcessDetectedEvent>();
  assert!(
    publish_exec_hit(&tx, placeholder_hit(), EXEC_EVENT_CAP),
    "an accepted hit is published"
  );
}

/// The semantic that makes the EXEC lane safe to bound: a shed is not an
/// error.
///
/// The caller marks `scan_dirty` *before* publishing, so a lost fast-path
/// event costs one poll interval and then the whole table is re-emitted.
/// Reporting "published" for a shed is what lets the loop carry on
/// instead of tearing the thread down over a dropped best-effort hint.
#[test]
fn a_shed_exec_hit_is_not_an_error() {
  let (tx, rx) = adsum_telemetry::QueueGauge::pair::<ProcessDetectedEvent>();
  let cap = EXEC_EVENT_CAP;
  // Saturate the backlog without draining, so the next send sheds.
  while matches!(
    tx.send_checked(ProcessDetectedEvent::Cleared, cap),
    SendChecked::Sent
  ) {}
  assert!(
    publish_exec_hit(&tx, placeholder_hit(), cap),
    "a shed best-effort hint must not read as a failure"
  );
  assert!(
    tx.gauge().dropped_total() > 0,
    "the shed must be counted, so the census can see a backlogged lane"
  );
  drop(rx);
}

/// A gone receiver is shutdown: the caller must stop, not spin.
#[test]
fn a_gone_receiver_stops_the_exec_lane() {
  let (tx, rx) = adsum_telemetry::QueueGauge::pair::<ProcessDetectedEvent>();
  drop(rx);
  assert!(
    !publish_exec_hit(&tx, placeholder_hit(), EXEC_EVENT_CAP),
    "a gone receiver is shutdown, not a shed"
  );
}

/// One placeholder hit (pid/entry are irrelevant to the lane).
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
    source: adsum_types::DetectSource::Automaton,
  }
}
