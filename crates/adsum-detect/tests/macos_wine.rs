//! macOS Wine detection: the wrapper process is only the loader, while the
//! game is the `.exe` found in its arguments or a Wine system path.

use std::sync::Arc;

use adsum_detect::db::DetectableActivity;
use adsum_detect::refresh::RefreshConfig;
use adsum_detect::scratch::ScanScratch;
use adsum_detect::server::ProcessServer;
use adsum_detect::types::{Exec, ProcessEventListeners, ScannedEntry};

fn server(arguments: Option<&str>) -> ProcessServer {
  let (tx, _rx) = adsum_telemetry::QueueGauge::pair();
  let executable = serde_json::json!({
    "name": "demo/game.exe",
    "is_launcher": false,
    "os": "win32",
    "arguments": arguments,
  });
  let entry: DetectableActivity = serde_json::from_value(serde_json::json!({
    "id": "1001",
    "name": "Demo Game",
    "hook": false,
    "executables": [executable],
    "aliases": []
  }))
  .expect("fixture parses");
  ProcessServer::new_with_custom(
    vec![Arc::new(ScannedEntry::from_owned(entry))],
    vec![],
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  )
}

fn classify(server: &ProcessServer, process: &Exec) -> Option<adsum_detect::types::ScannedHit> {
  let mut scratch = ScanScratch::default();
  let excl = server.exclusions_read();
  let mut obs_open = false;
  server.match_process(
    process,
    &server.bundle(),
    &excl,
    &mut obs_open,
    &mut scratch,
  )
}

fn wrapper(args: &str) -> Exec {
  Exec {
    pid: u64::from(std::process::id()),
    path: "/opt/homebrew/bin/wine64".to_string(),
    arguments: Some(args.to_string()),
    argument_spans: Vec::new(),
  }
}

#[test]
fn wine_wrapper_target_is_detected() {
  let server = server(None);
  let process = wrapper("/games/Demo/demo/game.exe");
  assert!(classify(&server, &process).is_some());
}

#[test]
fn wine_wrapper_target_with_required_argument_is_detected() {
  let server = server(Some("-windowed"));
  let process = wrapper("/games/Demo/demo/game.exe -windowed");
  assert!(classify(&server, &process).is_some());
}

/// A wrong target .exe or the wrong arguments must not become a hit.
#[test]
fn wine_wrapper_rejects_wrong_target_and_arguments() {
  let server = server(Some("-windowed"));
  assert!(classify(&server, &wrapper("/games/Demo/other.exe -windowed")).is_none());
  assert!(classify(&server, &wrapper("/games/Demo/demo/game.exe -open")).is_none());
}

#[test]
fn wine_wrapper_rejects_system_binary() {
  let server = server(None);
  let process = wrapper("/drive_c/windows/system32/explorer.exe");
  assert!(classify(&server, &process).is_none());
}
