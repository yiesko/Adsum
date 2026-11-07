//! Snapshot population regression tests.
//!
//! Validates the end-to-end path that populates `activities` in the
//! state snapshot: scanner detection -> bridge `broadcast_activity` ->
//! replay cache -> `persist_now` -> state file. A snapshot with an
//! empty activities array while games are detected means this chain
//! is broken somewhere.
//!
//! Lives in `tests/unit/` (dedicated folder) so it can reach
//! `pub(crate)` items (`shared_for_test`, `Shared` fields,
//! `router::broadcast_activity`, `snapshot::persist_now`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use adsum_protocol::commands;
use adsum_types::{AppId, DetectSource, SocketId};

use crate::bridge::{Shared, shared_for_test};
use crate::handoff::ScannedGame;
use crate::replay::ReplayCache;
use crate::router::generic_payload;
use crate::snapshot::{state_activities, state_servers};
use crate::state::{StateActivity, StateSnapshot};

/// Builds a `Shared` like `shared_for_test` but with `state_path` set,
/// so `persist_now` can write to a temp file the test controls.
fn shared_with_state(dir: &Path) -> Arc<Shared> {
  let mut shared = shared_for_test();
  {
    let sole = Arc::get_mut(&mut shared).expect("sole owner of a fresh shared");
    sole.state_path = Some(dir.join("adsum-state-0"));
    sole.app_version = "0.50.0-rc3-test".to_string();
    sole.json_port = 1337;
    sole.msgpack_port = 1338;
  }
  shared
}

/// One generic presence payload for app `app-1`, pid `7`.
fn sample_payload() -> Arc<commands::CachedActivity> {
  let game = ScannedGame {
    id: AppId::from("app-1"),
    name: Arc::from("Frostpeak"),
    pid: 7,
    start: 0,
    source: DetectSource::SteamAppId,
    process_start_ms: None,
  };
  generic_payload(&game).expect("builds")
}

/// `state_activities` extracts name, application_id and pid from the
/// replay cache, so a populated cache yields non-empty snapshot
/// activities. An empty cache yields an empty vec.
#[test]
fn state_activities_extracts_from_cache() {
  let mut cache: ReplayCache = HashMap::new();
  let payload = sample_payload();
  cache.insert(SocketId::from("app-1"), (Arc::clone(&payload), 1));

  let activities = state_activities(&cache);
  assert_eq!(activities.len(), 1);
  let activity = &activities[0];
  assert_eq!(activity.name.as_deref(), Some("Frostpeak"));
  assert_eq!(activity.application_id.as_deref(), Some("app-1"));
  assert_eq!(activity.pid, Some(7));
  assert_eq!(activity.detection_source.as_deref(), Some("steam-app-id"));

  let empty = state_activities(&ReplayCache::new());
  assert!(empty.is_empty(), "empty cache yields empty activities");
}

/// `broadcast_activity` inserts into the replay cache and marks the
/// snapshot dirty. After a publish, `persist_now` must write a
/// snapshot whose `activities` is non-empty.
#[tokio::test]
async fn broadcast_activity_marks_dirty_and_caches() {
  let dir = std::env::temp_dir().join(format!("adsum-test-broadcast-{}", std::process::id()));
  // Clear leftovers from a failed prior run before recreating.
  let _ = std::fs::remove_dir_all(&dir);
  let _ = std::fs::create_dir_all(&dir);
  let shared = shared_with_state(&dir);

  shared.broadcast_activity(sample_payload(), SocketId::from("app-1"));

  assert!(
    shared.dirty.load(std::sync::atomic::Ordering::Relaxed),
    "publish must mark snapshot dirty"
  );
  assert_eq!(
    shared.cache.lock().unwrap_or_else(|e| e.into_inner()).len(),
    1,
    "publish must cache the activity"
  );

  let _ = std::fs::remove_dir_all(&dir);
}

/// `persist_now` writes a snapshot whose `activities` array contains
/// every cached entry. A populated cache must never produce an empty
/// activities array.
#[tokio::test]
async fn persist_now_writes_snapshot_with_activities() {
  let dir = std::env::temp_dir().join(format!("adsum-test-persist-{}", std::process::id()));
  // Clear leftovers from a failed prior run before recreating.
  let _ = std::fs::remove_dir_all(&dir);
  let _ = std::fs::create_dir_all(&dir);
  let shared = shared_with_state(&dir);

  shared.broadcast_activity(sample_payload(), SocketId::from("app-1"));
  shared.persist_now().await;

  let path = dir.join("adsum-state-0");
  let content = std::fs::read_to_string(&path).expect("snapshot written");
  let snapshot: serde_json::Value = serde_json::from_str(&content).expect("parses");

  assert_eq!(
    snapshot.get("appVersion").and_then(|v| v.as_str()),
    Some("0.50.0-rc3-test"),
    "app version stamped"
  );
  let activities = snapshot.get("activities").expect("activities key");
  assert!(activities.is_array(), "activities is an array");
  assert_eq!(
    activities.as_array().unwrap().len(),
    1,
    "one cached activity becomes one snapshot activity"
  );
  let activity = &activities[0];
  assert_eq!(
    activity.get("name").and_then(|v| v.as_str()),
    Some("Frostpeak"),
    "activity name preserved"
  );
  assert_eq!(
    activity.get("application_id").and_then(|v| v.as_str()),
    Some("app-1"),
    "application id preserved"
  );
  assert_eq!(activity.get("pid").and_then(|v| v.as_u64()), Some(7));

  let _ = std::fs::remove_dir_all(&dir);
}

/// `persist_now` writes an empty activities array when the cache is
/// empty. That is correct, not a bug: nothing detected means nothing
/// to report.
#[tokio::test]
async fn persist_now_writes_empty_activities_when_cache_empty() {
  let dir = std::env::temp_dir().join(format!("adsum-test-empty-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  let _ = std::fs::create_dir_all(&dir);
  let shared = shared_with_state(&dir);

  shared.persist_now().await;

  let path = dir.join("adsum-state-0");
  let content = std::fs::read_to_string(&path).expect("snapshot written");
  let snapshot: serde_json::Value = serde_json::from_str(&content).expect("parses");

  let activities = snapshot.get("activities").expect("activities key");
  assert!(activities.is_array(), "activities is an array");
  assert_eq!(
    activities.as_array().unwrap().len(),
    0,
    "empty cache yields empty activities array"
  );

  let _ = std::fs::remove_dir_all(&dir);
}

/// `persist_task` dirty-gating: when nothing was published, `dirty`
/// stays false and no write happens. A publish flips it true.
#[tokio::test]
async fn dirty_gating() {
  let shared = shared_for_test();

  assert!(
    !shared.dirty.load(std::sync::atomic::Ordering::Relaxed),
    "fresh shared is not dirty"
  );

  shared.broadcast_activity(sample_payload(), SocketId::from("app-1"));
  assert!(
    shared.dirty.load(std::sync::atomic::Ordering::Relaxed),
    "publish flips dirty to true"
  );

  shared
    .dirty
    .store(false, std::sync::atomic::Ordering::Relaxed);
  assert!(
    !shared.dirty.load(std::sync::atomic::Ordering::Relaxed),
    "consume can reset dirty"
  );
}

/// `StateSnapshot::new` stamps appVersion, timestamp, servers and
/// activities, and the whole thing serializes to the shape consumers
/// expect (the snapshot is write-only on disk: `Serialize` only, so the
/// round trip is checked against the JSON body, not a typed rebuild).
#[test]
fn state_snapshot_round_trip() {
  let servers = state_servers(1337, 1338, Some(6463));
  let activity = StateActivity {
    socket_id: "app-1".to_string(),
    name: Some("Frostpeak".to_string()),
    application_id: Some("app-1".to_string()),
    pid: Some(7),
    start_time: None,
    detection_source: Some(std::borrow::Cow::Borrowed("steam-app-id")),
    detect_latency_ms: None,
  };
  let snapshot = StateSnapshot::new("0.50.0-rc3", servers, vec![activity]);

  let json: serde_json::Value = serde_json::to_value(&snapshot).expect("serializes");

  assert_eq!(
    json.get("appVersion").and_then(|v| v.as_str()),
    Some("0.50.0-rc3")
  );
  let activities = json
    .get("activities")
    .and_then(|v| v.as_array())
    .expect("array");
  assert_eq!(activities.len(), 1);
  assert_eq!(
    activities[0].get("name").and_then(|v| v.as_str()),
    Some("Frostpeak")
  );
  assert_eq!(
    activities[0].get("socketId").and_then(|v| v.as_str()),
    Some("app-1")
  );
  assert!(
    json.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0) > 0,
    "timestamp set"
  );
}

/// The scanner pump thread translates scanner events into `ProcInput`
/// and sends them to the bridge. This simulates one detection event
/// flowing through `proc_pump`, then `broadcast_activity`, then the
/// replay cache.
#[tokio::test]
async fn proc_pump_detected_flows_to_cache() {
  let dir = std::env::temp_dir().join(format!("adsum-test-pump-{}", std::process::id()));
  // Clear leftovers from a failed prior run before recreating.
  let _ = std::fs::remove_dir_all(&dir);
  let _ = std::fs::create_dir_all(&dir);
  let shared = shared_with_state(&dir);

  // Simulate what proc_pump does on ProcInput::Detected.
  let game = ScannedGame {
    id: AppId::from("app-1"),
    name: Arc::from("Frostpeak"),
    pid: 7,
    start: 0,
    source: DetectSource::SteamAppId,
    process_start_ms: None,
  };
  if let Some(payload) = generic_payload(&game) {
    shared.broadcast_activity(payload, SocketId::from("app-1"));
  }

  assert_eq!(
    shared.cache.lock().unwrap_or_else(|e| e.into_inner()).len(),
    1,
    "scanner detection lands in the replay cache"
  );
  assert!(
    shared.dirty.load(std::sync::atomic::Ordering::Relaxed),
    "snapshot marked dirty after detection"
  );

  let _ = std::fs::remove_dir_all(&dir);
}
