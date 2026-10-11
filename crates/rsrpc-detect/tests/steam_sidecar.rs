//! A store id is not proof that the process is the game.
//!
//! Steam ends an update by running the app's script phase and by setting up
//! the Proton prefix, and it hands those the game's environment —
//! `SteamAppId` included. Under Proton they even run with the environ hidden
//! from a service context, so the id reaches the scanner by way of the
//! command line: "AppId 4000 for pid … from command line (environ
//! unreadable)", one second after Steam committed 3005 updated files, while
//! the user was only downloading.
//!
//! The process that did it was named once the fix was deployed and the
//! download repeated: Steam's own `reaper`, under its `ubuntu12_32` runtime
//! directory, launched at the end of the update with the app's AppId. It is
//! the regression this file now pins by name, and the reason the rule is the
//! executable rather than a list: `reaper` is a plain word a game could ship
//! as its own binary.

use std::sync::Arc;

use rsrpc_detect::db::DetectableActivity;
use rsrpc_detect::refresh::RefreshConfig;
use rsrpc_detect::scan::ScanScratch;
use rsrpc_detect::server::ProcessServer;
use rsrpc_detect::types::{DetectSource, Exec, ProcessEventListeners};

/// Steam AppId of the game being updated (Garry's Mod).
const APP_ID: &str = "4000";

/// The basenames in [`STEAM_SIDECAR_BASENAMES`] cover the known Steam
/// launchers. These do not — and are the ones only the executable rule can
/// catch: a script interpreter, a runtime, the client itself, and the one
/// that actually did it. The short unknown process that announced Garry's
/// Mod while 3005 files were being committed was Steam's own `reaper`, from
/// its `ubuntu12_32` runtime, launched at the end of the update with the
/// app's AppId — which is why the rule does not depend on the list being
/// complete, and why `reaper` is deliberately *not* on it: a plain word can
/// be a game's binary, while the executable check cannot be fooled by a name
/// at all.
const UNLISTED_HELPERS: &[&str] = &[
  "reaper",
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
    // The tier under test, not just the outcome: the entry is `win32`
    // only, so the Proton automaton is a second route to the same hit,
    // and a test that stops at the name passes either way. Asserting the
    // provenance is what makes a rejection in the id tier visible here
    // instead of being silently papered over by a fallback.
    assert_eq!(
      hit.source,
      DetectSource::SteamAppId,
      "{argv0} must be the AppId tier answering, not a fallback"
    );
  }
}

/// Two entries that collide by construction, exactly as
/// `applications/detectable` declares them: Minecraft declares the bare
/// `>java` (the real entry also declares `>javaw.exe`, left out because
/// the collision under test is the one it takes part in), and Illarion
/// declares `illarion/jre/bin/java.exe`, whose automaton pattern
/// *contains* Minecraft's. No store ids, so only the automaton can
/// answer — the same exclusion the other fixture relies on.
fn colliding_exe_server() -> ProcessServer {
  let (tx, _rx) = rsrpc_telemetry::QueueGauge::pair();
  let entries: Vec<DetectableActivity> = serde_json::from_value(serde_json::json!([
    {
      "id": "1",
      "name": "Minecraft",
      "hook": false,
      "executables": [{"name": ">java", "is_launcher": false, "os": ""}],
      "aliases": []
    },
    {
      "id": "2",
      "name": "Illarion",
      "hook": false,
      "executables": [{"name": "illarion/jre/bin/java.exe", "is_launcher": false, "os": ""}],
      "aliases": []
    }
  ]))
  .expect("fixture parses");
  ProcessServer::new_with_custom(
    entries.into_iter().map(Arc::new).collect(),
    vec![],
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  )
}

/// A hit rejected on the boundary must not hide the match that does name
/// the path.
///
/// The automaton reports the earliest-*ending* match, and a different
/// entry's shorter declared name can sit inside the path: Minecraft's
/// `java` pattern is the interior of `illarion/jre/bin/java.exe`, and it
/// ends earlier than Illarion's, so it is the one returned. Rejecting it
/// is right — the path does not end with `java` — but falling through to
/// the next variant with it throws away the match that *does* name the
/// process. That is what the boundary check did right after it landed:
/// the loop moved on instead of asking the automaton for the rest of the
/// variant's matches, and this process stopped being detected at all
/// (worse: before the boundary check it was announced as Minecraft).
///
/// Same entry order as the database, so the pattern order is realistic.
#[test]
fn a_rejected_match_does_not_hide_the_one_that_names_the_path() {
  let server = colliding_exe_server();
  let bundle = server.bundle();
  let mut scratch = ScanScratch::default();
  let mut variants: [String; 5] = Default::default();
  let mut reversed = String::new();
  let mut obs_open = false;

  let hit = server
    .match_process(
      &process("/opt/illarion/jre/bin/java.exe", ""),
      &bundle,
      &mut variants,
      &mut reversed,
      &mut obs_open,
      &mut scratch,
    )
    .expect(".../illarion/jre/bin/java.exe should read as Illarion, not be lost");
  assert_eq!(
    hit.entry.name,
    "Illarion".into(),
    "the interior `java` match that was rejected belongs to Minecraft"
  );
  assert_eq!(
    hit.source,
    DetectSource::Automaton,
    "no store id is in play, so only the automaton can answer"
  );
}

/// The other half of the rule above: a path that ends with the *short*
/// declared name still matches it, and the entry that declares it is the
/// one announced — `java` declares the executable *named* `java`, so the
/// path has to end with exactly that, not with `java.exe`.
///
/// The two tests together pin that the boundary rule filters matches
/// without starving the variant they came from: the long name that
/// contains the short one, and the short name on its own.
#[test]
fn the_short_declared_name_still_matches_a_path_ending_with_it() {
  let server = colliding_exe_server();
  let bundle = server.bundle();
  let mut scratch = ScanScratch::default();
  let mut variants: [String; 5] = Default::default();
  let mut reversed = String::new();
  let mut obs_open = false;

  for argv0 in ["/opt/minecraft/java", "/opt/minecraft/jre/bin/java"] {
    let hit = server
      .match_process(
        &process(argv0, ""),
        &bundle,
        &mut variants,
        &mut reversed,
        &mut obs_open,
        &mut scratch,
      )
      .unwrap_or_else(|| panic!("{argv0} should read as Minecraft"));
    assert_eq!(hit.entry.name, "Minecraft".into(), "{argv0}");
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

/// A server whose one entry is reachable **only** by the executables it
/// declares: no store id at all, so the id tier has nothing to look up
/// and the automaton is the only tier that can answer. (The fixture
/// above carries the real Steam id, which lets that tier answer instead
/// — a different rule, already pinned there.)
///
/// The executables carry an empty `os` on purpose: a pattern for an
/// executable with no OS lands in the native automaton on every
/// platform, where a `win32` one is reachable only through the Proton
/// automaton and only on Linux. The tier under test is the same either
/// way — both probes run the one shared variant loop — but the CI tests
/// on Windows and macOS too, and there the Proton automaton is never
/// built at all. The names are still the real ones
/// `applications/detectable` declares for this game.
fn exe_only_server() -> ProcessServer {
  let (tx, _rx) = rsrpc_telemetry::QueueGauge::pair();
  let entry: DetectableActivity = serde_json::from_value(serde_json::json!({
    "id": "356879032584896512",
    "name": "Garry's Mod",
    "hook": false,
    "executables": [
      {"name": "hl2.exe", "is_launcher": false, "os": ""},
      {"name": "garrysmod/hl2.exe", "is_launcher": false, "os": ""},
      {"name": "bin/gmad.exe", "is_launcher": false, "os": ""},
      {"name": "win64/gmod.exe", "is_launcher": false, "os": ""},
    ],
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

/// The automaton matches a substring, not a path component: any process
/// whose path merely *contains* a declared executable name reads as the
/// game. `.../xhl2.exe` holds `hl2.exe`; so does `.../hl2.exe.txt`; so
/// does `.../hl2.exe.evil`. Nothing about any of them says the game is
/// running.
///
/// No store id is in play here, so the id-tier corroboration that fixed
/// the inherited-AppId false positive cannot answer for this one: the
/// automaton is the tier that has to know the difference.
#[test]
fn a_path_that_merely_contains_a_declared_exe_name_is_not_the_game() {
  let server = exe_only_server();
  let bundle = server.bundle();
  let mut scratch = ScanScratch::default();
  let mut variants: [String; 5] = Default::default();
  let mut reversed = String::new();
  let mut obs_open = false;

  for argv0 in [
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/xhl2.exe",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/hl2.exe.txt",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/hl2.exe.evil",
  ] {
    let hit = server.match_process(
      &process(argv0, ""),
      &bundle,
      &mut variants,
      &mut reversed,
      &mut obs_open,
      &mut scratch,
    );
    assert!(
      hit.is_none(),
      "{argv0} only contains a declared name — it does not end with one, \
       so it names no executable the entry declares"
    );
  }
}

/// The other half of the rule, with no store id in play: the game's own
/// process keeps its detection through the automaton itself. A boundary
/// rule that is too tight is a silent false negative — a legitimate game
/// that simply stops being announced — so this half is pinned as hard as
/// the rejection above.
///
/// The last case is why the check reads the string that was actually
/// probed: `hl2_64.exe` reaches the automaton as its 64-bit-stripped
/// variant `hl2.exe`, and the boundary has to hold for the variant.
#[test]
fn the_automaton_still_names_the_games_own_process() {
  let server = exe_only_server();
  let bundle = server.bundle();
  let mut scratch = ScanScratch::default();
  let mut variants: [String; 5] = Default::default();
  let mut reversed = String::new();
  let mut obs_open = false;

  for argv0 in [
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/hl2.exe",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/garrysmod/hl2.exe",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/win64/gmod.exe",
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/bin/gmad.exe",
    // Probed as its stripped variant `.../hl2.exe`.
    "/C:/Program Files (x86)/Steam/steamapps/common/GarrysMod/hl2_64.exe",
  ] {
    let hit = server
      .match_process(
        &process(argv0, ""),
        &bundle,
        &mut variants,
        &mut reversed,
        &mut obs_open,
        &mut scratch,
      )
      .unwrap_or_else(|| panic!("{argv0} should read as the game"));
    assert_eq!(hit.entry.name, "Garry's Mod".into());
    assert_eq!(
      hit.source,
      DetectSource::Automaton,
      "{argv0} must be the automaton answering, not a heuristic falling back"
    );
  }
}
