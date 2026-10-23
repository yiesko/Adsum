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
#[path = "../tests/unit/heroic.rs"]
mod tests;
