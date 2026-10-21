//! Heroic `-epicapp` name -> Epic namespace, from the cached Legendary
//! library: older Legendary releases omit `-epicsandboxid`, but Heroic's
//! `store_cache/legendary_library.json` still maps the app name. Read-only.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rustc_hash::FxHashMap;
use serde::Deserialize;

use crate::server::ProcessServer;

/// Override for non-standard Heroic installs (and hermetic tests): the
/// config root; nothing else is consulted when it is set.
const HEROIC_ROOT_ENV: &str = "ADSUM_HEROIC_ROOT";
/// The Legendary library cache inside Heroic's config root.
const LIBRARY_FILE: &str = "store_cache/legendary_library.json";
/// Real files are ~5.4 MB; anything past this is corrupt or hostile.
const MAX_LIBRARY_BYTES: u64 = 64 * 1024 * 1024;

/// Lowercased Heroic app name -> lowercased Epic namespace, refreshed when
/// the cached Legendary library changes. Discovery is lazy: the first scan
/// tick parses, construction only stats for the file.
#[derive(Debug, Default)]
pub(crate) struct HeroicApps {
  /// Library file to watch, once discovered.
  path: Option<PathBuf>,
  /// `(mtime, len)` seen at the last completed attempt. `None` before the
  /// first read and after a transient error, so the next tick retries.
  fingerprint: Option<(Option<SystemTime>, u64)>,
  /// Test/tooling injection: a manual map is never replaced by a refresh.
  manual: bool,
  /// Lowercased app name -> lowercased namespace.
  apps: FxHashMap<Box<str>, Box<str>>,
}

impl HeroicApps {
  /// Discover the library path only; the first [`refresh_if_stale`](Self::refresh_if_stale)
  /// parses it, so construction and startup stay cheap.
  pub(crate) fn discover() -> Self {
    Self {
      path: discover_path(),
      ..Default::default()
    }
  }

  /// Build from explicit pairs (hermetic tests and tooling): the map is
  /// manual, so no refresh overwrites it.
  pub(crate) fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
    let mut heroic = Self {
      manual: true,
      ..Default::default()
    };
    for (app_name, namespace) in pairs {
      insert_pair(&mut heroic.apps, &app_name, &namespace);
    }
    heroic
  }

  /// Namespace for a Heroic `-epicapp` name, matched case-insensitively.
  /// The map is already lowercase, so only an uppercase query allocates.
  pub(crate) fn namespace_for(&self, app_name: &str) -> Option<&str> {
    if let Some(namespace) = self.apps.get(app_name) {
      return Some(namespace.as_ref());
    }
    if !app_name.bytes().any(|b| b.is_ascii_uppercase()) {
      return None;
    }
    self
      .apps
      .get(app_name.to_ascii_lowercase().as_str())
      .map(AsRef::as_ref)
  }

  /// Revalidate the library: once the path is known, one stat per tick and
  /// a parse only when the `(mtime, len)` fingerprint moves; while absent,
  /// cheap discovery probes. Failed reads/parses keep the last good map.
  pub(crate) fn refresh_if_stale(&mut self) {
    if self.manual {
      return;
    }
    if self.path.is_none() {
      self.path = discover_path();
      self.fingerprint = None;
    }
    let Some(path) = self.path.as_deref() else {
      return;
    };
    let meta = match std::fs::metadata(path) {
      Ok(meta) => meta,
      Err(_) => {
        // Vanished (Heroic removed, Flatpak moved): forget the path so a
        // later tick rediscovers it; the last good map stays until then.
        self.path = None;
        self.fingerprint = None;
        return;
      }
    };
    let fingerprint = (meta.modified().ok(), meta.len());
    if self.fingerprint.as_ref() == Some(&fingerprint) {
      return;
    }
    // Record the attempt before reading: a corrupt or over-cap file is
    // retried only when its fingerprint moves, never every tick.
    self.fingerprint = Some(fingerprint);
    if meta.len() > MAX_LIBRARY_BYTES {
      tracing::debug!(
        "[Process Scanner] Heroic library over size cap, skipping: {}",
        path.display()
      );
      return;
    }
    let bytes = match read_library(path, meta.len()) {
      Ok(bytes) => bytes,
      Err(error) if error.kind() == std::io::ErrorKind::FileTooLarge => {
        tracing::debug!(
          "[Process Scanner] Heroic library over size cap, skipping ({error}): {}",
          path.display()
        );
        return;
      }
      Err(error) => {
        // Transient (permissions, I/O, replaced mid-read): clear the
        // fingerprint so the next tick tries again even if the stat moves
        // back to the same value; the last good map stays.
        self.fingerprint = None;
        tracing::debug!(
          "[Process Scanner] Heroic library unreadable ({error}): {}",
          path.display()
        );
        return;
      }
    };
    match parse_library(&bytes) {
      Ok(apps) => {
        tracing::debug!("[Process Scanner] Heroic library: {} app names", apps.len());
        self.apps = apps;
      }
      Err(error) => tracing::debug!(
        "[Process Scanner] Heroic library unparsed ({error}), keeping the last map: {}",
        path.display()
      ),
    }
  }
}

/// Candidate library files, most specific first: an explicit root is
/// exclusive, otherwise XDG, the conventional config dir, then Flatpak.
/// Covers Linux; the override works on any platform.
fn candidate_paths(
  root: Option<&OsStr>,
  xdg: Option<&OsStr>,
  home: Option<&OsStr>,
) -> Vec<PathBuf> {
  if let Some(root) = root.filter(|root| !root.is_empty()) {
    return vec![PathBuf::from(root).join(LIBRARY_FILE)];
  }
  let mut roots = Vec::with_capacity(3);
  if let Some(xdg) = xdg.filter(|xdg| !xdg.is_empty()) {
    roots.push(PathBuf::from(xdg).join("heroic"));
  }
  if let Some(home) = home.filter(|home| !home.is_empty()) {
    roots.push(PathBuf::from(home).join(".config/heroic"));
    roots.push(PathBuf::from(home).join(".var/app/com.heroicgameslauncher.hgl/config/heroic"));
  }
  roots
    .into_iter()
    .map(|root| root.join(LIBRARY_FILE))
    .collect()
}

/// Discover the cached library file from the environment, if it exists.
fn discover_path() -> Option<PathBuf> {
  candidate_paths(
    std::env::var_os(HEROIC_ROOT_ENV).as_deref(),
    std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    std::env::var_os("HOME").as_deref(),
  )
  .into_iter()
  .find(|path| path.is_file())
}

/// Read the library with a hard byte cap: the post-read length check also
/// covers a grow/replace between the stat and the read.
fn read_library(path: &Path, len: u64) -> std::io::Result<Vec<u8>> {
  use std::io::Read;
  let file = std::fs::File::open(path)?;
  let mut bytes = Vec::with_capacity(len.min(MAX_LIBRARY_BYTES) as usize);
  file.take(MAX_LIBRARY_BYTES + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > MAX_LIBRARY_BYTES {
    return Err(std::io::Error::new(
      std::io::ErrorKind::FileTooLarge,
      "Heroic library over size cap",
    ));
  }
  Ok(bytes)
}

/// Only `app_name` and `namespace` are deserialized; every other field in
/// the 5.4 MB library is skipped without allocation.
#[derive(Deserialize)]
struct LibraryFile<'a> {
  #[serde(default, borrow)]
  library: Vec<LibraryEntry<'a>>,
}

/// One library entry, borrowed from the input where possible. `null`,
/// missing and blank fields are skipped instead of failing the whole file.
#[derive(Deserialize)]
struct LibraryEntry<'a> {
  #[serde(default, borrow)]
  app_name: Option<Cow<'a, str>>,
  #[serde(default, borrow)]
  namespace: Option<Cow<'a, str>>,
}

/// Parse a `legendary_library.json` body into the lowercased app map.
fn parse_library(bytes: &[u8]) -> Result<FxHashMap<Box<str>, Box<str>>, serde_json::Error> {
  let doc: LibraryFile<'_> = serde_json::from_slice(bytes)?;
  let mut apps = FxHashMap::default();
  apps.reserve(doc.library.len());
  for entry in doc.library {
    if let (Some(app_name), Some(namespace)) =
      (entry.app_name.as_deref(), entry.namespace.as_deref())
    {
      insert_pair(&mut apps, app_name, namespace);
    }
  }
  Ok(apps)
}

/// Insert one lowercased pair, dropping an entry missing either side.
fn insert_pair(apps: &mut FxHashMap<Box<str>, Box<str>>, app_name: &str, namespace: &str) {
  let app_name = app_name.trim();
  let namespace = namespace.trim();
  if app_name.is_empty() || namespace.is_empty() {
    return;
  }
  apps.insert(
    app_name.to_ascii_lowercase().into_boxed_str(),
    namespace.to_ascii_lowercase().into_boxed_str(),
  );
}

impl ProcessServer {
  /// Replace the Heroic app map (hermetic test/tooling seam): the injected
  /// map is manual and survives every refresh.
  pub fn set_heroic_apps(&self, pairs: impl IntoIterator<Item = (String, String)>) {
    *self.heroic_apps.write().unwrap_or_else(|e| e.into_inner()) = HeroicApps::from_pairs(pairs);
  }

  /// Revalidate the cached Legendary library (one stat unless it moved).
  /// Called once per scan tick, alongside the Steam refresh.
  pub(crate) fn refresh_heroic_apps(&self) {
    self
      .heroic_apps
      .write()
      .unwrap_or_else(|e| e.into_inner())
      .refresh_if_stale();
  }
}

#[cfg(test)]
mod tests {
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
}
