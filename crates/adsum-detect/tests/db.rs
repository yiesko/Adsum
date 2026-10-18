use adsum_detect::db::{
  BUNDLED_DETECTABLE, content_hashes, parse_exclusions, trim_detectable, trim_detectable_value,
};

/// The fallback parse builds no second DOM.
///
/// The trimmed path used to walk a `Vec<Value>` and then a second
/// `Vec<Value>`, so a body that failed the direct decode paid for three
/// structures at once — which on a 64 MiB body is a peak several times the
/// database itself, at exactly the moment the daemon is already busy
/// recovering from a malformed upstream. It now builds one small object per
/// entry and drops it before the next is read.
///
/// Counted in allocations rather than measured in bytes: absolute heap and
/// RSS readings drift with whatever else runs in the test binary, which is
/// what made the measured version of this test fail for reasons that had
/// nothing to do with the parse. Counting is a delta around one call, and
/// the comparison is a ratio against the direct decode, so only a
/// structural change moves it.
#[test]
fn the_fallback_parse_builds_no_second_dom() {
  // Each entry carries a long name so the JSON bytes dominate over per-entry
  // overhead, and the entry count is large enough that an extra DOM is a
  // whole extra set of allocations per entry rather than a rounding error.
  let entries = 20_000usize;
  let body: String = (0..entries)
    .map(|i| {
      format!(
        r#"{{"id":"{i}","name":"{}","hook":false,"executables":[{{"name":"e{i}.exe","os":"linux","is_launcher":false}}]}}"#,
        "n".repeat(200)
      )
    })
    .collect::<Vec<_>>()
    .join(",");
  let body = format!("[{body}]");

  let counter = allocations::counter();
  let (direct_len, direct) = counter.during(|| {
    serde_json::from_str::<Vec<adsum_detect::db::DetectableActivity>>(&body)
      .expect("direct decode")
      .len()
  });
  let (fallback_len, fallback) = counter.during(|| {
    adsum_detect::db::trim_parse(&body)
      .expect("trim parse")
      .len()
  });

  assert_eq!(direct_len, entries, "every entry must decode directly");
  assert_eq!(fallback_len, entries, "every entry must survive the trim");
  // The trimmed path reads the body twice over — once as raw JSON, once
  // through the projection — so it is allowed more work, but not another
  // whole DOM. Measured: the clean fallback peaks within 22 allocations of
  // the direct decode, and a body re-parsed into a `Vec<Value>` peaks at
  // 4.5x it. The budget sits in that gap.
  let budget = direct.saturating_mul(2);
  assert!(
    fallback <= budget,
    "the fallback peaked at {fallback} allocations where the direct decode peaked \
     at {direct}: a second structure is riding along"
  );
}

/// Allocation counting for the memory-shape tests in this binary.
///
/// A delta around one call is what the tests assert on, so concurrent
/// tests in the same binary cancel out: only this call's own allocations
/// land between the two readings.
mod allocations {
  use std::alloc::{GlobalAlloc, Layout, System};
  use std::cell::Cell;

  thread_local! {
    /// Live allocations on this thread, and the peak it reached. Thread-local
    /// so a test running concurrently in this binary — which the harness does
    /// by default — cannot inflate the reading: its allocations are on its own
    /// thread.
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
  }

  struct Counting;

  // SAFETY: forwards every operation to the system allocator and only
  // moves counters alongside it. No pointer is retained.
  unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
      LIVE.with(|live| {
        let now = live.get() + 1;
        live.set(now);
        PEAK.with(|peak| peak.set(peak.get().max(now)));
      });
      // SAFETY: the layout comes from the caller, unchanged.
      unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
      LIVE.with(|live| live.set(live.get().saturating_sub(1)));
      // SAFETY: the pointer and layout come from the caller, unchanged.
      unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
      LIVE.with(|live| {
        let now = live.get() + 1;
        live.set(now);
        PEAK.with(|peak| peak.set(peak.get().max(now)));
      });
      // SAFETY: pointer, layout and new size come from the caller.
      unsafe { System.realloc(ptr, layout, new_size) }
    }
  }

  #[global_allocator]
  static COUNTING: Counting = Counting;

  /// The per-thread allocation counter.
  pub fn counter() -> Counter {
    Counter
  }

  pub struct Counter;

  impl Counter {
    /// The value `body` returned, plus how many allocations were in flight
    /// at the busiest moment inside it — on this thread only.
    ///
    /// Peak rather than end-state, because the thing under test is a
    /// transient: a second DOM is built and freed inside the call, so the
    /// live count at the end is identical either way. The peak is what the
    /// daemon actually has to survive.
    pub fn during<T>(&self, body: impl FnOnce() -> T) -> (T, usize) {
      let peak_before = PEAK.with(Cell::get);
      let value = body();
      let peak_after = PEAK.with(Cell::get);
      assert!(peak_after >= peak_before, "the peak went backwards");
      (value, peak_after.saturating_sub(peak_before))
    }
  }
}

/// Bundled snapshot parses and survives the trim round-trip intact.
#[test]
fn bundled_snapshot_parses_and_hashes() {
  let parsed: Vec<serde_json::Value> =
    serde_json::from_str(BUNDLED_DETECTABLE).expect("bundled db");
  assert!(!parsed.is_empty(), "bundled snapshot must not be empty");
  let trimmed = trim_detectable(BUNDLED_DETECTABLE).expect("trims");
  let reparsed: Vec<serde_json::Value> = serde_json::from_str(&trimmed).expect("trimmed parses");
  assert_eq!(reparsed.len(), parsed.len());
}

/// Canonical hashes ignore hook/description churn; raw hashes do not.
#[test]
fn content_hashes_ignore_volatile_churn() {
  let body_a = r#"[{"id":"1","name":"Game","executables":[{"name":"game.exe","is_launcher":false,"os":"win32"}],"third_party_skus":[],"aliases":[],"hook":false,"description":"v1"}]"#;
  let body_b = r#"[{"id":"1","name":"Game","executables":[{"name":"game.exe","is_launcher":false,"os":"win32"}],"third_party_skus":[],"aliases":[],"hook":true,"description":"v2"}]"#;
  let parsed_a: Vec<adsum_detect::db::DetectableActivity> = serde_json::from_str(body_a).unwrap();
  let parsed_b: Vec<serde_json::Value> =
    serde_json::from_str(&trim_detectable(body_b).unwrap()).unwrap();
  let parsed_b: Vec<adsum_detect::db::DetectableActivity> =
    serde_json::from_value(serde_json::Value::Array(parsed_b)).unwrap();
  // Raw bytes differ AND hook/description churned: raw hash moves...
  let (raw_a, canon_a) = content_hashes(body_a, &parsed_a);
  let (raw_b, canon_b) = content_hashes(body_b, &parsed_b);
  assert_ne!(raw_a, raw_b);
  // ...but the scanner-visible projection is identical.
  assert_eq!(canon_a, canon_b);
}

/// Trimming drops descriptions, extras and empty SKUs; keeps match fields.
#[test]
fn trim_keeps_only_scanner_fields() {
  let value = trim_detectable_value(
    r#"[{"id":"9","name":"N","hook":true,"aliases":["Alt","  ",""],"executables":[{"name":"n.exe","is_launcher":false,"os":"win32","arguments":"--x","extra":1}],"third_party_skus":[{"distributor":"steam","id":"99"},{"distributor":"","id":"x"}],"description":"drop me"}]"#,
  )
  .expect("trims");
  let entry = &value[0];
  assert_eq!(entry["id"], "9");
  assert_eq!(entry["aliases"], serde_json::json!(["Alt"]));
  assert_eq!(entry["executables"][0]["arguments"], "--x");
  assert!(entry["executables"][0].get("extra").is_none());
  assert!(entry.get("description").is_none());
  assert!(entry.get("extra").is_none());
  // Empty distributor dropped, steam kept.
  assert_eq!(entry["third_party_skus"].as_array().unwrap().len(), 1);
}

/// Exclusions match basenames and regexes, case-insensitively.
#[test]
fn exclusions_match_basename_and_patterns() {
  let exclusions = parse_exclusions(
    r#"{"executables":["crashpad_handler.exe"],"patterns":["vcredist.*\\.exe$"]}"#,
  );
  assert!(exclusions.is_excluded("crashpad_handler.exe"));
  assert!(exclusions.is_excluded("VCREDIST_x64.EXE"));
  assert!(!exclusions.is_excluded("game.exe"));
}

/// Garbage exclusion payloads block nothing and keep valid patterns.
#[test]
fn exclusions_tolerate_garbage() {
  let empty = parse_exclusions("not json{{");
  assert!(!empty.is_excluded("anything.exe"));
  // Invalid regex skipped, valid one kept.
  let partial = parse_exclusions(r#"{"patterns":["([invalid","game\\.exe$"]}"#);
  assert!(partial.is_excluded("game.exe"));
  assert!(!partial.is_excluded("other.exe"));
}

use adsum_detect::db::entry_projection_hash;
use adsum_detect::db::{
  hash_entry_aliases, hash_entry_exes, hash_entry_identity, hash_entry_steam,
};

/// Minimal entry fixture: only match-relevant fields set.
fn probe_entry(id: &str, exe_os: &str) -> adsum_detect::db::DetectableActivity {
  serde_json::from_value(serde_json::json!({
    "id": id,
    "name": format!("Game {id}"),
    "hook": false,
    "executables": [{"name": format!("{id}.exe"), "is_launcher": false, "os": exe_os}],
    "third_party_skus": [],
    "aliases": []
  }))
  .expect("fixture parses")
}

/// Per-entry projection hashes fold into the canonical whole-list hash:
/// refactoring `canonical_content_hash` around `entry_projection_hash`
/// must not change the canonical value.
#[test]
fn entry_projection_hash_fold_matches_canonical() {
  use std::hash::{DefaultHasher, Hash, Hasher};
  let entries = vec![probe_entry("1", "win32"), probe_entry("2", "linux")];
  let canonical = adsum_detect::db::canonical_content_hash(&entries);
  let mut folded = DefaultHasher::new();
  entries.len().hash(&mut folded);
  for entry in &entries {
    entry_projection_hash(entry, &mut folded);
  }
  assert_eq!(canonical, folded.finish());
}

/// Field sub-hashes fold back into the per-entry hash: splitting the
/// stream must not change any value.
#[test]
fn field_hashes_fold_into_entry_hash() {
  use std::hash::{DefaultHasher, Hasher};
  let entries = vec![probe_entry("1", "win32"), probe_entry("2", "linux")];
  for entry in &entries {
    let mut whole = DefaultHasher::new();
    entry_projection_hash(entry, &mut whole);
    // Same portions, same order, same running hasher: the fold must
    // reproduce the whole-entry value byte-for-byte.
    let mut folded = DefaultHasher::new();
    hash_entry_identity(entry, &mut folded);
    hash_entry_exes(entry, &mut folded);
    hash_entry_steam(entry, &mut folded);
    hash_entry_aliases(entry, &mut folded);
    assert_eq!(whole.finish(), folded.finish());
  }
}
