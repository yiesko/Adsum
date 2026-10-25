//! Steam install-dir corroboration: a demo binary absent from the database
//! is still the game when its AppId owns the install dir it runs under.
//!
//! Hermetic: each test builds a temp Steam root and injects its library map,
//! so nothing here depends on a real Steam installation.

use std::path::PathBuf;
use std::sync::Arc;

use adsum_detect::db::{DetectableActivity, parse_exclusions};
use adsum_detect::refresh::RefreshConfig;
use adsum_detect::scratch::ScanScratch;
use adsum_detect::server::ProcessServer;
use adsum_detect::types::{DetectSource, Exec, ProcessEventListeners};
use adsum_steam::SteamLibraries;

/// Nonexistent pid: `/proc` reads fail, so tests never inherit a
/// `SteamAppId` from the runner's environment.
const NO_PID: u64 = u64::MAX;
/// Steam AppId of the VEIN demo install.
const VEIN_APP_ID: &str = "2082470";
/// The demo binary: not declared by the database (which only has
/// `win64/vein.exe`), but under the AppId's install dir.
const DEMO_PATH: &str = "/s:/steamapps/common/VeinDemo/Vein/Binaries/Win64/VeinDemo-Win64-Test.exe";

/// Unique scratch dir, removed on drop, panic or not.
struct Scratch {
  path: PathBuf,
}

impl Scratch {
  /// Fresh unique temp dir for one test (pre-cleaned).
  fn new(tag: &str) -> Self {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
      "adsum-detect-steam-install-{}-{}-{}",
      std::process::id(),
      tag,
      COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("scratch dir");
    Self { path }
  }

  /// Path inside the scratch root.
  fn join(&self, name: &str) -> PathBuf {
    self.path.join(name)
  }
}

impl Drop for Scratch {
  /// Remove the scratch dir (best-effort, panic-safe).
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

/// Fake Steam root whose manifest maps AppId 2082470 to `VeinDemo`.
fn fake_steam_root(tag: &str) -> Scratch {
  let root = Scratch::new(tag);
  let apps = root.join("steamapps");
  std::fs::create_dir_all(&apps).expect("steamapps");
  let folders = format!(
    "\"libraryfolders\"\n{{\n\"0\"\n{{\n\"path\"\t\t\"{}\"\n}}\n}}",
    root.path.to_string_lossy().replace('\\', "\\\\")
  );
  std::fs::write(apps.join("libraryfolders.vdf"), folders).expect("libraryfolders");
  let manifest =
    format!("\"AppState\"\n{{\n\"appid\"\t\t\"{VEIN_APP_ID}\"\n\"installdir\"\t\t\"VeinDemo\"\n}}");
  std::fs::write(
    apps.join(format!("appmanifest_{VEIN_APP_ID}.acf")),
    manifest,
  )
  .expect("manifest");
  root
}

/// VEIN as the bundled database declares it: AppId plus one executable the
/// demo does not ship.
fn vein_entry() -> DetectableActivity {
  serde_json::from_value(serde_json::json!({
    "id": "1124360440122720266",
    "name": "VEIN",
    "hook": true,
    "executables": [{"name": "win64/vein.exe", "is_launcher": false, "os": "win32"}],
    "third_party_skus": [{"distributor": "steam", "id": VEIN_APP_ID, "sku": null}],
    "aliases": []
  }))
  .expect("fixture parses")
}

/// A second known store entry, AppId 999999, owning `other.exe`.
fn other_entry() -> DetectableActivity {
  serde_json::from_value(serde_json::json!({
    "id": "999999",
    "name": "OTHER GAME",
    "hook": true,
    "executables": [{"name": "other.exe", "is_launcher": false, "os": "win32"}],
    "third_party_skus": [{"distributor": "steam", "id": "999999", "sku": null}],
    "aliases": []
  }))
  .expect("fixture parses")
}

/// A fixture entry that declares a launcher alongside the game binary.
fn launcher_entry() -> DetectableActivity {
  serde_json::from_value(serde_json::json!({
    "id": "1124360440122720266",
    "name": "VEIN",
    "hook": true,
    "executables": [
      {"name": "win64/vein.exe", "is_launcher": false, "os": "win32"},
      {"name": "VeinDemo.exe", "is_launcher": true, "os": "win32"},
    ],
    "third_party_skus": [{"distributor": "steam", "id": VEIN_APP_ID, "sku": null}],
    "aliases": []
  }))
  .expect("fixture parses")
}

/// Server carrying exactly the given entries.
fn server(entries: Vec<DetectableActivity>) -> ProcessServer {
  let (tx, _rx) = adsum_telemetry::QueueGauge::pair();
  ProcessServer::new_with_custom(
    entries.into_iter().map(Arc::new).collect(),
    vec![],
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  )
}

/// Classify one process through the public scanner seam, using a
/// nonexistent pid on purpose so no `/proc` environment leaks in.
fn classify(
  server: &ProcessServer,
  path: &str,
  arguments: &str,
) -> Option<adsum_detect::types::ScannedHit> {
  let bundle = server.bundle();
  let excl = server.exclusions_read();
  let mut obs_open = false;
  let mut scratch = ScanScratch::default();
  server.match_process(
    &Exec {
      pid: NO_PID,
      path: path.to_string(),
      arguments: Some(arguments.to_string()),
    },
    &bundle,
    &excl,
    &mut obs_open,
    &mut scratch,
  )
}

/// The demo binary is not in the database, but the AppId's install dir
/// corroborates it: the motivating VEIN Demo case.
#[test]
fn demo_executable_outside_the_database_is_detected() {
  let root = fake_steam_root("demo");
  let server = server(vec![vein_entry()]);
  server.set_steam_libraries(SteamLibraries::from_root(&root.path));

  let hit = classify(&server, DEMO_PATH, "VeinDemo-Win64-Test.exe AppId=2082470")
    .expect("the demo must be detected from the install dir");
  assert_eq!(hit.entry.name, "VEIN".into());
  assert_eq!(hit.source, DetectSource::SteamAppId);
}

/// Steam's helpers inherit the game's AppId and stay refused: the install
/// dir never covers `reaper`, the runtime, the client or the prefix.
#[test]
fn steam_helpers_with_the_same_app_id_stay_invisible() {
  let root = fake_steam_root("helpers");
  let server = server(vec![vein_entry()]);
  server.set_steam_libraries(SteamLibraries::from_root(&root.path));

  for (path, arguments) in [
    ("/usr/lib/steam/bin/reaper", "reaper AppId=2082470"),
    (
      "/mnt/hdd/steamapps/common/SteamLinuxRuntime_4/pressure-vessel/bin/pressure-vessel",
      "pressure-vessel AppId=2082470",
    ),
    (
      "/mnt/hdd/steamapps/compatdata/2082470/pfx/drive_c/Program Files (x86)/Steam/steam.exe",
      "steam.exe AppId=2082470",
    ),
    (
      "/mnt/hdd/steamapps/compatdata/2082470/pfx/drive_c/windows/system32/winedevice.exe",
      "winedevice.exe AppId=2082470",
    ),
  ] {
    assert!(
      classify(&server, path, arguments).is_none(),
      "{path} carrying AppId {VEIN_APP_ID} must not read as the game"
    );
  }
}

/// Prerequisite installers run by Steam under the game's AppId are not
/// the game just because they sit under its install dir; no exclusion list
/// is involved here.
#[test]
fn prerequisite_installers_under_the_install_dir_are_not_the_game() {
  let root = fake_steam_root("redist");
  let server = server(vec![vein_entry()]);
  server.set_steam_libraries(SteamLibraries::from_root(&root.path));

  for path in [
    "/s:/steamapps/common/VeinDemo/_CommonRedist/vcredist_x64.exe",
    "/s:/steamapps/common/VeinDemo/DirectX/dxsetup.exe",
    "/s:/steamapps/common/VeinDemo/Engine/Extras/Redist/en-us/UEPrereqSetup_x64.exe",
  ] {
    assert!(
      classify(&server, path, "installer.exe AppId=2082470").is_none(),
      "{path} carrying AppId {VEIN_APP_ID} must not read as the game"
    );
  }
}

/// An excluded crash reporter under the install dir stays excluded: the
/// new evidence does not outrank the exclusion list.
#[test]
fn an_excluded_crash_reporter_under_the_install_dir_is_not_the_game() {
  let root = fake_steam_root("crash");
  let server = server(vec![vein_entry()]);
  server.set_steam_libraries(SteamLibraries::from_root(&root.path));
  server.set_exclusions(parse_exclusions(
    r#"{"executables":["crashreportclient.exe"]}"#,
  ));

  assert!(
    classify(
      &server,
      "/s:/steamapps/common/VeinDemo/Engine/Binaries/Win64/CrashReportClient.exe",
      "CrashReportClient.exe AppId=2082470",
    )
    .is_none(),
    "an excluded crash reporter must stay invisible under the install dir"
  );
}

/// A declared launcher under the install dir is not promoted by the new
/// evidence; the launcher guard still applies.
#[test]
fn a_declared_launcher_under_the_install_dir_is_not_the_game() {
  let root = fake_steam_root("launcher");
  let server = server(vec![launcher_entry()]);
  server.set_steam_libraries(SteamLibraries::from_root(&root.path));

  assert!(
    classify(
      &server,
      "/s:/steamapps/common/VeinDemo/VeinDemo.exe",
      "VeinDemo.exe AppId=2082470",
    )
    .is_none(),
    "a declared launcher must never be accepted by install-dir corroboration"
  );
}

/// A *known* different AppId is not corroborated by B's path, and the
/// library tier cannot use B's own path as independent evidence: the
/// undeclared executable stays refused.
#[test]
fn a_known_different_app_id_does_not_make_the_library_tier_accept_an_undeclared_executable() {
  let root = fake_steam_root("other-id");
  let server = server(vec![vein_entry(), other_entry()]);
  server.set_steam_libraries(SteamLibraries::from_root(&root.path));

  // Direct proof that the predicate compares the matched AppId rather
  // than merely checking presence: only B corroborates B's install dir.
  let mut scratch = String::new();
  assert!(
    server.steam_install_tail_matches_app_id_into(DEMO_PATH, VEIN_APP_ID, &mut scratch),
    "the demo path belongs to {VEIN_APP_ID}"
  );
  assert!(
    !server.steam_install_tail_matches_app_id_into(DEMO_PATH, "999999", &mut scratch),
    "the demo path must not corroborate another known AppId"
  );

  assert!(
    classify(&server, DEMO_PATH, "VeinDemo-Win64-Test.exe AppId=999999").is_none(),
    "the library tier must not corroborate a path-derived AppId with that same path"
  );
}

/// Without an AppId the library tier derives B from the path, but that
/// AppId is not independent evidence: the undeclared demo stays refused.
#[test]
fn a_library_tail_alone_does_not_accept_an_undeclared_executable() {
  let root = fake_steam_root("no-app-id");
  let server = server(vec![vein_entry()]);
  server.set_steam_libraries(SteamLibraries::from_root(&root.path));

  assert!(
    classify(&server, DEMO_PATH, "VeinDemo-Win64-Test.exe").is_none(),
    "a path-derived AppId must not corroborate the same path"
  );
}
