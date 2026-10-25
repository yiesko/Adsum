use rustc_hash::FxHashSet;

use super::*;

/// Exclusions fixture with one executable and no patterns.
fn full_exclusions() -> Exclusions {
  Exclusions {
    executables: FxHashSet::from_iter(["setup.exe".to_string()]),
    patterns: regex::RegexSet::empty(),
  }
}

/// Empty exclusions fixture (blocks nothing).
fn empty_exclusions() -> Exclusions {
  Exclusions {
    executables: FxHashSet::default(),
    patterns: regex::RegexSet::empty(),
  }
}

/// Empty fetches never wipe the active exclusion set.
#[test]
fn empty_fetch_never_wipes_active_set() {
  // A malformed upstream (error page served with HTTP 200) parses to
  // an empty set: keep serving the previous one instead of unblocking
  // installers and crash reporters for an hour.
  assert!(!may_replace_exclusions(
    &full_exclusions(),
    &empty_exclusions()
  ));
  assert!(may_replace_exclusions(
    &full_exclusions(),
    &full_exclusions()
  ));
  assert!(may_replace_exclusions(
    &empty_exclusions(),
    &empty_exclusions()
  ));
}
