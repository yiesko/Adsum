use super::*;

/// v1 caches (backslash prefix keys on Windows) are ignored once and
/// rescanned instead of reused empty.
#[test]
fn v1_cache_is_ignored() {
  let dir = std::env::temp_dir().join(format!("adsum-steam-test-cachev1-{}", std::process::id()));
  let _ = std::fs::create_dir_all(&dir);
  let file = dir.join("cache.json");
  std::fs::write(
      &file,
      r#"{"version":1,"libraries":{"/lib":{"fingerprint":{"dir_mtime_ms":1,"manifests":1,"newest_manifest_ms":1,"compat_mtime_ms":0},"dirs":{"/lib/steamapps/common/game/":"1"}}}}"#,
    )
    .expect("v1 cache");
  assert!(
    load_cache_from(&file).is_empty(),
    "v1 cache must be ignored"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// The current schema version still loads through the same path.
#[test]
fn current_cache_version_loads() {
  let dir = std::env::temp_dir().join(format!("adsum-steam-test-cachev2-{}", std::process::id()));
  let _ = std::fs::create_dir_all(&dir);
  let file = dir.join("cache.json");
  let body = serde_json::json!({
    "version": STEAM_CACHE_VERSION,
    "libraries": {
      "/lib": {
        "fingerprint": {"dir_mtime_ms": 1, "manifests": 1, "newest_manifest_ms": 1, "compat_mtime_ms": 0},
        "dirs": {"/lib/steamapps/common/game/": "1"}
      }
    }
  });
  std::fs::write(&file, body.to_string()).expect("v2 cache");
  let cached = load_cache_from(&file);
  assert_eq!(
    cached["/lib"].dirs.get("/lib/steamapps/common/game/"),
    Some(&"1".to_string())
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// The conflict sentinel must survive the cache: dropping it would let
/// a same-tail prefix in another library answer after a restart, while
/// a fresh scan refuses both.
#[test]
fn conflicting_install_dir_sentinel_survives_the_cache() {
  let dir = std::env::temp_dir().join(format!(
    "adsum-steam-test-cache-sentinel-{}",
    std::process::id()
  ));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("cache dir");
  let file = dir.join("cache.json");

  let fingerprint = Fingerprint {
    dir_mtime_ms: 1,
    manifests: 2,
    newest_manifest_ms: 1,
    compat_mtime_ms: 0,
  };
  let mut libraries = SteamLibraries::default();
  libraries.set_cache_file(file.clone());
  libraries
    .fingerprints
    .insert("/lib-a".into(), fingerprint.clone());
  libraries.fingerprints.insert("/lib-b".into(), fingerprint);
  libraries.dirs.insert(
    "/lib-a/steamapps/common/game/".into(),
    CONFLICTING_APPID.into(),
  );
  libraries
    .dirs
    .insert("/lib-b/steamapps/common/game/".into(), "222".into());

  save_cache(&libraries);
  let cached = load_cache_from(&file);
  assert_eq!(
    cached["/lib-a"].dirs.get("/lib-a/steamapps/common/game/"),
    Some(&CONFLICTING_APPID.to_string()),
    "the empty sentinel must round-trip"
  );

  let mut reloaded = SteamLibraries::default();
  for entry in cached.values() {
    reloaded.dirs.extend(entry.dirs.clone());
  }
  assert!(
    reloaded
      .match_install_tail("/x/steamapps/common/Game/game.exe")
      .is_none(),
    "the reloaded sentinel must keep the shared tail ambiguous"
  );

  let _ = std::fs::remove_dir_all(&dir);
}
