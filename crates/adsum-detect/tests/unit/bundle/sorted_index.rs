use crate::bundle::SortedIndex;

/// The build's tie rule, pinned on the packed representation: a stable
/// sort keeps insertion order among equal keys and `dedup_by` keeps the
/// FIRST of each run, so the canonical entry (inserted before its
/// aliases) wins — identical to the `or_insert` the table replaced.
#[test]
fn first_inserted_index_wins_duplicate_keys() {
  let index = SortedIndex::build(vec![
    ("dup".into(), 7),
    ("aaa".into(), 1),
    ("dup".into(), 9),
    ("aaa".into(), 3),
  ]);
  assert_eq!(index.get("dup"), Some(7));
  assert_eq!(index.get("aaa"), Some(1));
  assert_eq!(index.len(), 2, "duplicates collapse");
}

/// Round trip through the arena: keys come back byte-identical and in
/// sorted order, indexes travel with them, and the packed layout is
/// exactly one contiguous arena plus (start, end, index) triples.
#[test]
fn packed_arena_round_trips_keys_and_indexes() {
  let index = SortedIndex::build(vec![
    ("portal 2".into(), 440),
    ("dota 2".into(), 570),
    ("half life 2".into(), 220),
  ]);
  // Sorted by key: dota < half < portal.
  assert_eq!(
    index.iter().collect::<Vec<_>>(),
    vec![("dota 2", 570), ("half life 2", 220), ("portal 2", 440)]
  );
  assert_eq!(index.get("half life 2"), Some(220));
  assert!(index.contains_key("portal 2"));
  assert!(!index.contains_key("portal"));
  assert_eq!(index.get(""), None);
  // The arena is the exact concatenation (no separators, no slack):
  // this is what makes the binary search probe contiguous bytes.
  assert_eq!(&*index.arena, "dota 2half life 2portal 2");
  assert_eq!(
    &*index.slots,
    &[(0, 6, 570), (6, 17, 220), (17, 25, 440)][..]
  );
}

/// The empty table stays empty and answers nobody (the blind-generation
/// fallback builds exactly this shape).
#[test]
fn an_empty_index_answers_nobody() {
  let index = SortedIndex::build(Vec::new());
  assert_eq!(index.len(), 0);
  assert_eq!(index.get("anything"), None);
  assert_eq!(index.iter().count(), 0);
  let default = SortedIndex::default();
  assert_eq!(default.get("anything"), None);
}
