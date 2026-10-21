//! Epic `-epicapp` fallback, end to end through the scanner.

use std::sync::Arc;

use adsum_detect::db::DetectableActivity;
use adsum_detect::refresh::RefreshConfig;
use adsum_detect::scan::ScanScratch;
use adsum_detect::server::ProcessServer;
use adsum_detect::types::{DetectSource, Exec, ProcessEventListeners, ScannedHit};

/// Heroic's app name for GTA V Enhanced (the `-epicapp` value).
const APP_NAME: &str = "8769e24080ea413b8ebca3f1b8c50951";
/// The Epic namespace the database stores for the same game.
const NAMESPACE: &str = "b0cd075465c44f87be3b505ac04a2e46";
/// The real Heroic launch path for GTA V Enhanced.
const PROCESS_PATH: &str = "/s:/heroic/gtavenhanced/gta5_enhanced.exe";
/// A second launch path that names nothing GTA's entries declare.
const PLAY_PATH: &str = "/s:/heroic/gtavenhanced/playgtav.exe";
/// GTA V Enhanced's database id.
const GTA_ID: &str = "1421192030440263791";
/// A second entry, same executable, distinct id and Epic namespace.
const SECOND_ID: &str = "999";
const SECOND_NAMESPACE: &str = "ffffffffffffffffffffffffffffffff";
/// Not in the database: the sandbox tier must miss and fall through.
const UNKNOWN_NAMESPACE: &str = "00000000000000000000000000000000";

/// The fixture entries, both declaring the same executable so the app-name
/// mapping can only lose to the sandbox id by precedence, not by a miss.
fn gta_db() -> Vec<Arc<DetectableActivity>> {
  vec![
    Arc::new(activity(
      GTA_ID,
      "Grand Theft Auto V Enhanced",
      "B0CD075465C44F87BE3B505AC04A2E46",
    )),
    Arc::new(activity(
      SECOND_ID,
      "Grand Theft Auto V Enhanced (second entry)",
      "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
    )),
  ]
}

/// One database entry as the fixture declares it.
fn activity(id: &str, name: &str, epic_id: &str) -> DetectableActivity {
  serde_json::from_value(serde_json::json!({
    "id": id,
    "name": name,
    "hook": false,
    "executables": [{
      "name": "grand theft auto v enhanced\\gta5_enhanced.exe",
      "os": "win32",
      "is_launcher": false
    }],
    "third_party_skus": [
      {"distributor": "epic", "id": epic_id}
    ],
    "aliases": []
  }))
  .expect("fixture parses")
}

/// Empty-database server fixture with a live event sender.
fn server() -> ProcessServer {
  let (tx, _rx) = adsum_telemetry::QueueGauge::pair();
  ProcessServer::new_with_custom(
    gta_db(),
    vec![],
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  )
}

/// Classify one process against the server's current generation.
fn classify_at(server: &ProcessServer, path: &str, arguments: &str) -> Option<ScannedHit> {
  let bundle = server.bundle();
  let mut variant_bufs: [String; 5] = Default::default();
  let mut reversed = String::with_capacity(256);
  let mut obs_open = false;
  let mut scratch = ScanScratch::default();
  server.match_process(
    &Exec {
      pid: u64::from(std::process::id()),
      path: path.to_string(),
      arguments: Some(arguments.to_string()),
    },
    &bundle,
    &mut variant_bufs,
    &mut reversed,
    &mut obs_open,
    &mut scratch,
  )
}

/// Classify the GTA Heroic process path.
fn classify(server: &ProcessServer, arguments: &str) -> Option<ScannedHit> {
  classify_at(server, PROCESS_PATH, arguments)
}

/// `-epicapp` alone resolves through Heroic's library to the Epic SKU.
#[test]
fn epic_app_fallback_resolves_the_heroic_namespace() {
  let server = server();
  server.set_heroic_apps([(APP_NAME.to_string(), NAMESPACE.to_string())]);
  let hit = classify(&server, &format!("-epicapp={APP_NAME}"))
    .expect("the Heroic mapping resolves the launch");
  assert_eq!(hit.source, DetectSource::EpicAppId);
  assert_eq!(&*hit.entry.id, GTA_ID);
}

/// Without a Heroic mapping the `-epicapp` fallback leaves the short
/// Heroic launch undetected: no tier names it on its own.
#[test]
fn epic_app_fallback_requires_a_heroic_mapping() {
  let server = server();
  server.set_heroic_apps(std::iter::empty());
  let hit = classify(&server, &format!("-epicapp={APP_NAME}"));
  assert!(hit.is_none(), "an unmapped app name must not produce a hit");
}

/// The sandbox id wins even when the app name maps to another entry that
/// also declares this executable: with the fallback first, `999` would win.
#[test]
fn sandbox_id_takes_precedence_over_the_app_name() {
  let server = server();
  server.set_heroic_apps([(APP_NAME.to_string(), SECOND_NAMESPACE.to_string())]);
  let hit = classify(
    &server,
    &format!("-epicsandboxid={NAMESPACE} -epicapp={APP_NAME}"),
  )
  .expect("the sandbox id resolves first");
  assert_eq!(hit.source, DetectSource::EpicAppId);
  assert_eq!(
    &*hit.entry.id, GTA_ID,
    "the app-name mapping must not beat the sandbox id"
  );
}

/// A sandbox id miss falls through to the app-name mapping.
#[test]
fn app_name_fallback_runs_after_an_unknown_sandbox_id() {
  let server = server();
  server.set_heroic_apps([(APP_NAME.to_string(), NAMESPACE.to_string())]);
  let hit = classify(
    &server,
    &format!("-epicsandboxid={UNKNOWN_NAMESPACE} -epicapp={APP_NAME}"),
  )
  .expect("the app name resolves when the sandbox id does not");
  assert_eq!(hit.source, DetectSource::EpicAppId);
  assert_eq!(&*hit.entry.id, GTA_ID);
}

/// The app-name mapping is only an id: it still has to be corroborated by
/// a declared executable in the process path.
#[test]
fn epic_app_fallback_requires_a_declared_executable() {
  let server = server();
  server.set_heroic_apps([(APP_NAME.to_string(), NAMESPACE.to_string())]);
  let hit = classify_at(&server, PLAY_PATH, &format!("-epicapp={APP_NAME}"));
  assert!(
    hit.is_none(),
    "an app-name hit must corroborate the path like the sandbox id does"
  );
}
