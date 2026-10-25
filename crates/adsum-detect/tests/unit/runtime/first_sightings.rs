use super::*;

/// A detected hit for `id` (pid is irrelevant here).
fn hit(id: &str) -> ScannedHit {
  ScannedHit {
    entry: Arc::new(ScannedEntry {
      id: id.into(),
      name: "Game".into(),
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

/// The set bounds memory instead of remembering every id a long-lived
/// daemon ever saw. Dropping it whole means a repeat INFO can surface
/// after churn — the accepted trade for constant memory.
#[test]
fn seen_ids_stay_bounded_across_id_churn() {
  let mut seen: HashSet<String> = HashSet::new();
  // Well past the cap, in distinct batches so the clear is reached
  // repeatedly rather than once.
  for batch in 0..(MAX_SEEN_IDS * 3 / 64 + 2) {
    let batch: Vec<ScannedHit> = (0..64).map(|i| hit(&format!("game-{batch}-{i}"))).collect();
    let _ = first_sightings(&mut seen, &batch);
    assert!(
      seen.len() <= MAX_SEEN_IDS,
      "seen grew to {} past the cap of {MAX_SEEN_IDS}",
      seen.len()
    );
  }
}

/// An id already seen yields no first sighting, and does not allocate a
/// key just to have the insert discard it.
#[test]
fn repeat_ids_are_not_first_sightings() {
  let mut seen: HashSet<String> = HashSet::new();
  let batch = vec![hit("a"), hit("b"), hit("a")];
  let first = first_sightings(&mut seen, &batch);
  assert_eq!(first.len(), 2);
  assert_eq!(first[0].entry.id.as_ref(), "a");
  assert_eq!(first[1].entry.id.as_ref(), "b");
  // Second pass over the same corpus: nothing is new.
  assert!(first_sightings(&mut seen, &batch).is_empty());
}
