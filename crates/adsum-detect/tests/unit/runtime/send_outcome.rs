use super::*;

/// A gone receiver is the only send outcome that ends a lane: everything
/// else is either delivered or shed-and-repaired.
#[test]
fn a_gone_receiver_is_the_only_stopping_outcome() {
  let (tx, rx) = adsum_telemetry::QueueGauge::pair::<ProcessDetectedEvent>();
  drop(rx);
  assert_eq!(
    deliver_snapshot(&tx, &[], &[], true),
    SnapshotDelivery::Closed,
    "a gone receiver stops the snapshot lane"
  );
  assert!(
    !publish_exec_hit(&tx, placeholder_hit(), EXEC_EVENT_CAP),
    "a gone receiver stops the exec lane"
  );
}

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
