use super::*;

/// The pair is 8 B, not 16: half the dense table.
#[test]
fn pattern_index_is_compact() {
  assert_eq!(std::mem::size_of::<PatternIndex>(), 8);
  // A pair of zeroes still carries its halves.
  let pair = PatternIndex([0, 0]);
  assert_eq!(pair.resolve(), (0, 0));
}

/// `resolve` widens without truncating, including at the extremes: a
/// lost high bit would index the wrong entry and mis-attribute a game.
#[test]
fn resolve_widens_without_truncation() {
  for (activity, exe) in [
    (0usize, 0usize),
    (1, 7),
    (u16::MAX as usize, u16::MAX as usize),
    (u32::MAX as usize - 1, u32::MAX as usize - 1),
  ] {
    let pair = pattern_index(activity, exe);
    assert_eq!(pair.resolve(), (activity, exe), "pair lost a high bit");
  }
}

/// An out-of-range pair saturates and asserts: it must never silently
/// truncate to a valid-looking index.
#[test]
#[should_panic(expected = "database too large")]
fn overflow_pair_asserts_instead_of_truncating() {
  let _ = pattern_index(usize::MAX, usize::MAX);
}
