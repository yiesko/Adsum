use super::*;
use crate::refresh::RefreshConfig;
use crate::types::ProcessEventListeners;
use std::sync::Arc;

/// A server whose current generation is observed across a custom edit:
/// the main list must survive as the same allocation, not be re-walked.
fn server() -> ProcessServer {
  let (tx, _rx) = adsum_telemetry::QueueGauge::pair();
  ProcessServer::new_with_custom(
    vec![],
    vec![],
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  )
}

fn activity(id: &str) -> DetectableActivity {
  serde_json::from_value(serde_json::json!({
    "id": id,
    "name": format!("Game {id}"),
    "hook": false,
    "executables": [{"name": format!("{id}.exe"), "is_launcher": false, "os": "win32"}],
    "third_party_skus": [],
    "aliases": []
  }))
  .expect("fixture parses")
}

/// A custom-append rebuild carries the main list over as a refcount bump:
/// the same allocation is shared by the old and new generation, so a scan
/// tick that loaded the old one keeps reading valid entries and the rebuild
/// never walks ~24k of them.
#[test]
fn custom_rebuild_shares_the_main_list() {
  let server = server();
  // Seed a real main list, not the empty one `server()` builds.
  let main: Vec<DetectableActivity> = (0..64).map(|i| activity(&i.to_string())).collect();
  server.update_main_detectables(main);

  let before = server.detectables.load();
  let before_ptr = Arc::as_ptr(&before.list);
  let before_len = before.list.len();
  assert!(before_len > 0, "main list should have entries");

  // Append an override: this rebuilds the whole generation.
  server.append_detectables(vec![activity("custom-1")]);

  let after = server.detectables.load();
  // Same allocation, not a copy: this is the whole point of the Arc.
  assert_eq!(
    Arc::as_ptr(&after.list),
    before_ptr,
    "main list was re-walked instead of shared"
  );
  assert_eq!(after.list.len(), before_len);
  // And the override landed.
  assert_eq!(after.custom.len(), 1);
}

/// Removing every override falls back to an empty custom list without
/// touching the main list either.
#[test]
fn remove_override_keeps_sharing_the_main_list() {
  let server = server();
  let main: Vec<DetectableActivity> = (0..8).map(|i| activity(&i.to_string())).collect();
  server.update_main_detectables(main);
  server.append_detectables(vec![activity("custom-x")]);

  let before_ptr = Arc::as_ptr(&server.detectables.load().list);
  server.remove_detectable_by_name("Game custom-x");
  let after = server.detectables.load();

  assert_eq!(Arc::as_ptr(&after.list), before_ptr, "main list re-copied");
  assert!(after.custom.is_empty(), "override should be gone");
}
