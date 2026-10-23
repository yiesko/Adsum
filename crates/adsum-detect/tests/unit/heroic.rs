use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use super::*;

/// An explicit root is exclusive: XDG and HOME are not consulted.
#[test]
fn candidate_paths_prefers_an_explicit_root() {
  let paths = candidate_paths(
    Some(OsStr::new("/heroic")),
    Some(OsStr::new("/xdg")),
    Some(OsStr::new("/home/me")),
  );
  assert_eq!(
    paths,
    [PathBuf::from("/heroic").join(LIBRARY_FILE)],
    "the override must be the only root"
  );
}

/// Without an override: XDG first, then `$HOME/.config`, then Flatpak.
#[test]
fn candidate_paths_orders_xdg_then_home_then_flatpak() {
  let paths = candidate_paths(None, Some(OsStr::new("/xdg")), Some(OsStr::new("/home/me")));
  assert_eq!(
    paths,
    [
      PathBuf::from("/xdg/heroic").join(LIBRARY_FILE),
      PathBuf::from("/home/me/.config/heroic").join(LIBRARY_FILE),
      PathBuf::from("/home/me")
        .join(".var/app/com.heroicgameslauncher.hgl/config/heroic")
        .join(LIBRARY_FILE),
    ]
  );
}

/// An empty override counts as unset, and missing env vars just drop their
/// candidates.
#[test]
fn candidate_paths_ignores_empty_overrides() {
  let paths = candidate_paths(
    Some(OsStr::new("")),
    Some(OsStr::new("")),
    Some(OsStr::new("/home/me")),
  );
  assert_eq!(
    paths,
    [
      PathBuf::from("/home/me/.config/heroic").join(LIBRARY_FILE),
      PathBuf::from("/home/me")
        .join(".var/app/com.heroicgameslauncher.hgl/config/heroic")
        .join(LIBRARY_FILE),
    ]
  );
  assert!(candidate_paths(None, None, None).is_empty());
}

/// Pairs are stored lowercased and looked up case-insensitively.
#[test]
fn from_pairs_lowercases_and_looks_up_case_insensitively() {
  let apps = HeroicApps::from_pairs([(
    "8769E24080EA413B8EBCA3F1B8C50951".to_string(),
    "B0CD075465C44F87BE3B505AC04A2E46".to_string(),
  )]);
  assert_eq!(
    apps.namespace_for("8769e24080ea413b8ebca3f1b8c50951"),
    Some("b0cd075465c44f87be3b505ac04a2e46")
  );
  assert_eq!(
    apps.namespace_for("8769E24080EA413B8EBCA3F1B8C50951"),
    Some("b0cd075465c44f87be3b505ac04a2e46")
  );
  assert_eq!(apps.namespace_for("some-other-app"), None);
}

/// Only complete `app_name`/`namespace` pairs survive the parse; blank
/// either side and the extra fields are ignored.
#[test]
fn parse_keeps_only_complete_non_empty_pairs() {
  let doc = br#"{
      "library": [
        {"app_name": "APP", "namespace": "NS", "is_installed": true},
        {"app_name": "NO_NS"},
        {"namespace": "NO_APP"},
        {"app_name": "", "namespace": "EMPTY_APP"},
        {"app_name": "EMPTY_NS", "namespace": ""},
        {"app_name": "  ", "namespace": "NS2"}
      ]
    }"#;
  let apps = parse_library(doc).expect("valid JSON parses");
  assert_eq!(apps.len(), 1);
  assert_eq!(apps.get("app").map(AsRef::as_ref), Some("ns"));
}

/// Explicit `null` fields must not sink the whole file: the valid entry
/// survives, and the null/incomplete ones are skipped.
#[test]
fn parse_survives_null_app_name_and_namespace() {
  let doc = br#"{
      "library": [
        {"app_name": "GOOD", "namespace": "NS"},
        {"app_name": null, "namespace": "NULL_APP"},
        {"app_name": "NULL_NS", "namespace": null},
        {"app_name": null, "namespace": null}
      ]
    }"#;
  let apps = parse_library(doc).expect("valid JSON parses");
  assert_eq!(apps.len(), 1);
  assert_eq!(apps.get("good").map(AsRef::as_ref), Some("ns"));
}

/// A length change is a change even when the mtime does not move: the
/// fingerprint is `(mtime, len)`, not mtime alone.
#[test]
fn refresh_detects_a_length_change_with_the_same_mtime() {
  let dir = std::env::temp_dir().join(format!("adsum-heroic-len-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("temp dir");
  let file = dir.join("legendary_library.json");
  std::fs::write(
    &file,
    r#"{"library":[{"app_name":"ONE","namespace":"NS1"}]}"#,
  )
  .expect("first write");
  set_mtime(&file, 1_000);
  let mut apps = HeroicApps {
    path: Some(file.clone()),
    ..Default::default()
  };
  apps.refresh_if_stale();
  assert_eq!(apps.namespace_for("one"), Some("ns1"));

  std::fs::write(
    &file,
    r#"{"library":[{"app_name":"ONE","namespace":"NS1"},{"app_name":"TWO","namespace":"NS2"}]}"#,
  )
  .expect("second write");
  set_mtime(&file, 1_000);
  apps.refresh_if_stale();
  assert_eq!(
    apps.namespace_for("two"),
    Some("ns2"),
    "the size must be part of the freshness fingerprint"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// Same mtime and same length means no re-read: a rewrite that only
/// changes bytes stays invisible until the stat moves. That is the
/// guarantee the fingerprint exists for.
#[test]
fn refresh_skips_a_rewrite_with_the_same_size_and_mtime() {
  let dir = std::env::temp_dir().join(format!("adsum-heroic-same-stat-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("temp dir");
  let file = dir.join("legendary_library.json");
  std::fs::write(
    &file,
    r#"{"library":[{"app_name":"ONE","namespace":"NS1"}]}"#,
  )
  .expect("first write");
  set_mtime(&file, 1_000);
  let first_len = std::fs::metadata(&file).expect("stat").len();
  let mut apps = HeroicApps {
    path: Some(file.clone()),
    ..Default::default()
  };
  apps.refresh_if_stale();
  assert_eq!(apps.namespace_for("one"), Some("ns1"));

  std::fs::write(
    &file,
    r#"{"library":[{"app_name":"TWO","namespace":"NS2"}]}"#,
  )
  .expect("second write");
  set_mtime(&file, 1_000);
  assert_eq!(
    std::fs::metadata(&file).expect("stat").len(),
    first_len,
    "the fixture must keep the size equal"
  );
  apps.refresh_if_stale();
  assert_eq!(
    apps.namespace_for("one"),
    Some("ns1"),
    "an unchanged stat must not re-read the file"
  );
  assert_eq!(apps.namespace_for("two"), None);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A file past the cap is stat'd but never read; its fingerprint stays so
/// the next tick does not even try again.
#[test]
fn refresh_stats_an_over_cap_file_without_reading_it() {
  let dir = std::env::temp_dir().join(format!("adsum-heroic-cap-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("temp dir");
  let path = dir.join("legendary_library.json");
  let file = std::fs::File::create(&path).expect("create sparse file");
  file
    .set_len(MAX_LIBRARY_BYTES + 1)
    .expect("sparse length past the cap");
  drop(file);
  let mut apps = HeroicApps {
    path: Some(path),
    ..Default::default()
  };
  apps.refresh_if_stale();
  assert!(apps.apps.is_empty(), "an over-cap file must not be parsed");
  let fingerprint = apps.fingerprint;
  assert!(
    fingerprint.is_some(),
    "an over-cap file must not be retried every tick"
  );
  apps.refresh_if_stale();
  assert_eq!(
    apps.fingerprint, fingerprint,
    "the stat is unchanged, so no retry"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// A transient read error is retried on the next tick: the fingerprint is
/// cleared, so an unchanged file is still attempted again.
#[test]
fn refresh_retries_after_a_transient_read_error() {
  let dir = std::env::temp_dir().join(format!("adsum-heroic-retry-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("temp dir");
  let path = dir.join("legendary_library.json");
  std::fs::create_dir(&path).expect("a directory stands in for an unreadable file");
  let mut apps = HeroicApps {
    path: Some(path),
    ..Default::default()
  };
  apps.refresh_if_stale();
  assert!(
    apps.fingerprint.is_none(),
    "a transient read error must be retried, not frozen until the stat moves"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// A moved mtime reparses; a failed parse keeps the last good map.
#[test]
fn refresh_follows_mtime_and_keeps_the_last_good_map() {
  let dir = std::env::temp_dir().join(format!("adsum-heroic-test-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("temp dir");
  let file = dir.join("legendary_library.json");
  std::fs::write(
    &file,
    r#"{"library":[{"app_name":"ONE","namespace":"NS1"}]}"#,
  )
  .expect("first write");
  set_mtime(&file, 1_000);
  let mut apps = HeroicApps {
    path: Some(file.clone()),
    ..Default::default()
  };
  apps.refresh_if_stale();
  assert_eq!(apps.namespace_for("one"), Some("ns1"));

  std::fs::write(
    &file,
    r#"{"library":[{"app_name":"TWO","namespace":"NS2"}]}"#,
  )
  .expect("second write");
  set_mtime(&file, 2_000);
  apps.refresh_if_stale();
  assert_eq!(apps.namespace_for("two"), Some("ns2"));
  assert_eq!(apps.namespace_for("one"), None, "the map is replaced");

  std::fs::write(&file, "not json").expect("corrupt write");
  set_mtime(&file, 3_000);
  apps.refresh_if_stale();
  assert_eq!(
    apps.namespace_for("two"),
    Some("ns2"),
    "a parse failure must keep the last good map"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// A vanished file forgets its path (a later tick rediscovers) but keeps
/// the last good map until then.
#[test]
fn refresh_forgets_a_vanished_path_but_keeps_the_map() {
  let dir = std::env::temp_dir().join(format!("adsum-heroic-vanish-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("temp dir");
  let file = dir.join("legendary_library.json");
  std::fs::write(
    &file,
    r#"{"library":[{"app_name":"ONE","namespace":"NS1"}]}"#,
  )
  .expect("write");
  set_mtime(&file, 1_000);
  let mut apps = HeroicApps {
    path: Some(file.clone()),
    ..Default::default()
  };
  apps.refresh_if_stale();
  assert_eq!(apps.namespace_for("one"), Some("ns1"));

  std::fs::remove_file(&file).expect("remove");
  apps.refresh_if_stale();
  assert!(apps.path.is_none(), "the path must be rediscovered later");
  assert_eq!(
    apps.namespace_for("one"),
    Some("ns1"),
    "the last good map stays until rediscovery"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// Injected maps are manual: a refresh must never replace them.
#[test]
fn injected_maps_ignore_refresh() {
  let mut apps = HeroicApps::from_pairs([("APP".to_string(), "NS".to_string())]);
  apps.refresh_if_stale();
  assert!(apps.path.is_none());
  assert_eq!(apps.namespace_for("app"), Some("ns"));
}

/// Pin one file's mtime so the refresh sees a distinct, deterministic
/// change instead of racing the filesystem clock.
fn set_mtime(path: &Path, secs: u64) {
  let file = std::fs::OpenOptions::new()
    .write(true)
    .open(path)
    .expect("open for mtime");
  file
    .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
    .expect("set mtime");
}
