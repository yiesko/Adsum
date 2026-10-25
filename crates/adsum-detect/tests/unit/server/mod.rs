use super::*;
use std::time::Duration;

use crate::runtime::cadence::scan_cadence;
use crate::runtime::lanes::{exec_hit_ignored, removed_slots};
use crate::runtime::workers::unpark_scan_wake;
use crate::types::{DetectSource, ScannedEntry, ScannedHit};

mod generation;
mod lifecycle;
mod scan_control;
mod telemetry;

/// Empty-database server with a live gauge sender for unit tests.
fn fixture_server() -> ProcessServer {
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

/// Single shared-generation custom entry (id 777).
fn custom_entry() -> DetectableActivity {
  custom_entry_named("777")
}
/// Custom entry with a caller-chosen id for concurrency tests.
fn custom_entry_named(id: &str) -> DetectableActivity {
  serde_json::from_value(serde_json::json!({
    "id": id,
    "name": format!("Shared Generation {id}"),
    "hook": true,
    "executables": [{"name": format!("shared-{id}.exe"), "is_launcher": false, "os": "win32"}],
  }))
  .expect("fixture parses")
}

/// A wide main database: widens the refresh's load->store window so a
/// concurrent append is actually likely to land inside it.
fn main_entries(count: usize) -> Vec<DetectableActivity> {
  (0..count)
    .map(|i| {
      serde_json::from_value(serde_json::json!({
        "id": format!("main-{i}"),
        "name": format!("Main {i}"),
        "hook": true,
        "executables": [{"name": format!("main-{i}.exe"), "is_launcher": false, "os": "win32"}],
      }))
      .expect("fixture parses")
    })
    .collect()
}
