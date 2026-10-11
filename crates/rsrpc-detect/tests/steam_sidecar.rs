//! A store id is not proof that the process is the game.
//!
//! Steam ends an update by running the app's script phase and by setting up
//! the Proton prefix, and it hands those the game's environment —
//! `SteamAppId` included. Under Proton they even run with the environ hidden
//! from a service context, so the id reaches the scanner by way of the
//! command line: "AppId 4000 for pid … from command line (environ
//! unreadable)", one second after Steam committed 3005 updated files, while
//! the user was only downloading. That is the shape this file pins.

use std::sync::Arc;

use rsrpc_detect::db::DetectableActivity;
use rsrpc_detect::refresh::RefreshConfig;
use rsrpc_detect::scan::ScanScratch;
use rsrpc_detect::server::ProcessServer;
use rsrpc_detect::types::{Exec, ProcessEventListeners};

/// Steam AppId of the game being updated (Garry's Mod).
const APP_ID: &str = "4000";

/// The basenames in [`STEAM_SIDECAR_BASENAMES`] cover the known Steam
/// launchers. These do not — and are the ones only the executable rule can
/// catch: a script interpreter, a runtime, the client itself. The short
/// unknown process that announced Garry's Mod while 3005 files were being
/// committed was almost certainly one of these, which is exactly why the
/// rule does not depend on the list being complete.
const UNLISTED_HELPERS: &[&str] = &[
  "sh",
  "dash",
  "bash",
  "python3",
  "python",
  "wine64",
  "wine64-preloader",
  "steam",
  "equibop",
];

/// Every helper the policy names, plus the ones it does not.
fn helpers() -> impl Iterator<Item = &'static str> {
  rsrpc_detect::scan::STEAM_SIDECAR_BASENAMES
    .into_iter()
    .chain(UNLISTED_HELPERS.iter().copied())
}

/// A process as the scanner reads it: `argv[0]` plus the argument string
/// the cmdline reader joins (space-separated, like `/proc/<pid>/cmdline`).
fn process(argv0: &str, arguments: &str) -> Exec {
  Exec {
    pid: u64::from(std::process::id()),
    path: argv0.to_string(),
    arguments: Some(arguments.to_string()),
  }
}

/// A server whose one entry is reachable only by its Steam AppId.
///
/// The executables are the real ones `applications/detectable` declares for
/// this store id, not a fixture invented to pass: they are what a Proton
/// `argv[0]` has to be checked against.
fn server() -> ProcessServer {
  let (tx, _rx) = rsrpc_telemetry::QueueGauge::pair();
  let entry: DetectableActivity = serde_json::from_value(serde_json::json!({
    "id": "356879032584896512",
    "name": "Garry's Mod",
    "hook": false,
    "executables": [
      {"name": "hl2.exe", "is_launcher": false, "os": "win32"},
      {"name": "garrysmod/hl2.exe", "is_launcher": false, "os": "win32"},
      {"name": "bin/gmad.exe", "is_launcher": false, "os": "win32"},
      {"name": "win64/gmod.exe", "is_launcher": false, "os": "win32"},
    ],
    "third_party_skus": [{"distributor": "steam", "id": APP_ID, "sku": null}],
    "aliases": []
  }))
  .expect("fixture parses");
  ProcessServer::new_with_custom(
    vec![Arc::new(entry)],
    vec![],
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  )
}

/// Steam's own helpers stay invisible even while they carry the game's
/// `SteamAppId` — this is the false positive, in the shape it was reported.
#[test]
fn a_steam_helper_carrying_the_games_app_id_is_not_the_game() {
  let server = server();
  let bundle = server.bundle();
  let mut scratch = ScanScratch::default();
  let mut variants: [String; 5] = Default::default();
  let mut reversed = String::new();
  let mut obs_open = false;

  for helper in helpers() {
    // The id travels on the command line, exactly where the scanner looks
    // when the environ is unreadable (sandboxed Proton runtimes).
    let process = process(
      &format!("/usr/lib/steam/bin/{helper}"),
      &format!("{helper} AppId={APP_ID}"),
    );
    let hit = server.match_process(
      &process,
      &bundle,
      &mut variants,
      &mut reversed,
      &mut obs_open,
      &mut scratch,
    );
    assert!(
      hit.is_none(),
      "{helper} carrying AppId {APP_ID} was announced as the game: \
       an inherited store id is Steam working on the game, not the game running"
    );
  }
}

/// The game's own process keeps its detection: under Proton its `argv[0]`
/// is a Windows path, which is what the declared names match onto.
///
/// Same AppId, same arguments, only `argv[0]` differs from the test above —
/// which is the whole point: the id cannot tell these apart, the executable
/// can.
#[test]
fn the_games_own_process_still_reads_as_the_game() {
  let server = server();
  let bundle = server.bundle();
  let mut scratch = ScanScratch::default();
  let mut variants: [String; 5] = Default::default();
  let mut reversed = String::new();
  let mut obs_open = false;

  for argv0 in [
    // Proton: backslashes rewritten, drive letter kept, leading `/` added.
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/hl2.exe",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/garrysmod/hl2.exe",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/win64/gmod.exe",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/bin/gmad.exe",
    // Native: the database also carries the relative name alone.
    "hl2.exe",
    "garrysmod/hl2.exe",
  ] {
    let process = process(argv0, &format!("hl2.exe AppId={APP_ID}"));
    let hit = server
      .match_process(
        &process,
        &bundle,
        &mut variants,
        &mut reversed,
        &mut obs_open,
        &mut scratch,
      )
      .unwrap_or_else(|| panic!("{argv0} should read as the game"));
    assert_eq!(hit.entry.name, "Garry's Mod".into());
  }
}

/// An entry with no declared executables is the case the id tier exists
/// for, and it keeps working: there is nothing to corroborate against, so
/// the store id stands on its own.
#[test]
fn an_entry_with_no_executables_stands_on_the_id_alone() {
  let (tx, _rx) = rsrpc_telemetry::QueueGauge::pair();
  let entry: DetectableActivity = serde_json::from_value(serde_json::json!({
    "id": "1",
    "name": "No Exes",
    "hook": false,
    "executables": [],
    "third_party_skus": [{"distributor": "steam", "id": "777", "sku": null}]
  }))
  .expect("fixture parses");
  let server = ProcessServer::new_with_custom(
    vec![Arc::new(entry)],
    vec![],
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  );
  let bundle = server.bundle();
  let mut scratch = ScanScratch::default();
  let mut variants: [String; 5] = Default::default();
  let mut reversed = String::new();
  let mut obs_open = false;

  let process = process("/opt/whatever/some-launcher", "some-launcher AppId=777");
  let hit = server
    .match_process(
      &process,
      &bundle,
      &mut variants,
      &mut reversed,
      &mut obs_open,
      &mut scratch,
    )
    .expect("an entry with no executables still wins on the id alone");
  assert_eq!(hit.entry.name, "No Exes".into());
}
