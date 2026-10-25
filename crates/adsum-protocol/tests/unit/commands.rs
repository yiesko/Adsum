use super::*;

/// Hostile command names (quotes, backslashes, control chars) escape
/// through serde instead of breaking the emergency frame, in both
/// shapes (error event and null event).
#[test]
fn error_fallback_escapes_hostile_commands() {
  let hostile = "x\"\\\n\x00y";
  let frame = error_fallback_json(hostile, Some("ERROR"));
  let parsed: serde_json::Value = serde_json::from_str(&frame).expect("fallback is valid JSON");
  assert_eq!(parsed["cmd"], hostile);
  assert_eq!(parsed["evt"], "ERROR");
  let null_frame = error_fallback_json(hostile, None);
  let null_parsed: serde_json::Value =
    serde_json::from_str(&null_frame).expect("null fallback is valid JSON");
  assert_eq!(null_parsed["cmd"], hostile);
  assert!(null_parsed["evt"].is_null());
}

/// The reply fallback is itself valid JSON: a failure to serialize the
/// real reply must never emit a broken frame to a lock-step client.
#[test]
fn error_fallback_stays_valid_json() {
  for evt in [None, Some("ERROR")] {
    let fallback = error_fallback_json("SET_ACTIVITY", evt);
    let parsed: Value = serde_json::from_str(&fallback).expect("fallback must be valid JSON");
    assert_eq!(parsed["cmd"], "SET_ACTIVITY");
    assert_eq!(parsed["evt"], evt.map_or(Value::Null, |e| e.into()));
  }
}

/// N20: the flood guard hashes with the workspace's `FxHashMap`, not
/// `std`'s `SipHash`.
///
/// The guard runs once per publish over `(String, u64)` keys — small,
/// and derived from our own parsed command rather than from an
/// attacker-chosen login, so SipHash's DoS resistance buys nothing here
/// and costs hashing on the hot path. Every other table in the
/// workspace already uses `FxHash`.
///
/// Structural, because the win is hashing cost: no behavioral assertion
/// can tell the two hashers apart, they just produce different bucket
/// orders. The helper's parameter type is the assertion — it stops
/// compiling if the field ever reverts.
#[test]
fn flood_guard_uses_the_workspace_hasher() {
  // Compiles only for a `FxHashMap`.
  fn assert_fx<K: Eq + std::hash::Hash, V>(_: &rustc_hash::FxHashMap<K, V>) {}
  let guard = RecentActivities::default();
  assert_fx(&guard.entries);
}
