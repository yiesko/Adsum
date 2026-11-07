//! On-disk persistence of OAuth refresh tokens.
//!
//! On-disk persistence of OAuth refresh tokens: one JSON file per
//! application under `adsum/oauth/` in the platform cache directory —
//! `$XDG_CACHE_HOME`, `HOME`/`.cache`, or `%USERPROFILE%\.cache` on Windows —
//! with mode 0600 on Unix. The cache directory is the only path the
//! sandboxed systemd unit can already write,
//! and deleting the file just costs one fresh browser authorization. Access
//! tokens are never written to disk; grants left by older versions in
//! `~/.config/adsum/oauth/` are not read.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistedSession {
  pub client_id: String,
  pub refresh_token: Option<String>,
  pub scopes: Vec<String>,
  pub expires: u64,
}

impl std::fmt::Debug for PersistedSession {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // The refresh token is a live credential: never render it.
    f.debug_struct("PersistedSession")
      .field("client_id", &self.client_id)
      .field("refresh_token", &"[REDACTED]")
      .field("scopes", &self.scopes)
      .field("expires", &self.expires)
      .finish()
  }
}

impl PersistedSession {
  pub fn is_expired(&self) -> bool {
    SystemTime::now()
      .duration_since(SystemTime::UNIX_EPOCH)
      .map(|d| d.as_secs() >= self.expires)
      .unwrap_or(true)
  }
}

fn store_dir() -> Option<PathBuf> {
  let base = std::env::var("XDG_CACHE_HOME")
    .ok()
    .map(PathBuf::from)
    .filter(|path| path.is_absolute())
    .or_else(home_cache)?;
  Some(base.join("adsum").join("oauth"))
}

/// `~/.cache` under the platform's profile variable: `HOME` everywhere but
/// Windows, where only `USERPROFILE` is guaranteed to exist.
fn home_cache() -> Option<PathBuf> {
  let profile = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
  std::env::var(profile)
    .ok()
    .map(|h| PathBuf::from(h).join(".cache"))
}

fn store_path(client_id: &str) -> Option<PathBuf> {
  // Only digits: client ids are validated snowflakes, and this keeps a
  // crafted id from escaping the storage directory.
  if client_id.is_empty() || !client_id.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  let dir = store_dir()?;
  Some(dir.join(format!("{client_id}.json")))
}

pub fn save(session: &PersistedSession) -> std::io::Result<()> {
  let path = store_path(&session.client_id)
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid client_id"))?;
  if let Some(parent) = path.parent() {
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(parent)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(parent)?;
  }
  let json = serde_json::to_string(session)
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
  // Temp sibling first, then rename: a crash leaves the previous grant
  // intact instead of a truncated file, and the token is created 0600
  // rather than being briefly readable while the mode is fixed.
  static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
  let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
  let tmp = path.with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
  {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).truncate(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    if let Err(err) = file
      .write_all(json.as_bytes())
      .and_then(|()| file.sync_all())
    {
      let _ = fs::remove_file(&tmp);
      return Err(err);
    }
  }
  if let Err(err) = fs::rename(&tmp, &path) {
    let _ = fs::remove_file(&tmp);
    return Err(err);
  }
  // The rename installs the temp file's 0600 mode; the explicit chmod is
  // defense in depth for a recycled stale temp path.
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
  }
  Ok(())
}

pub fn load(client_id: &str) -> Option<PersistedSession> {
  let path = store_path(client_id)?;
  let data = fs::read_to_string(&path).ok()?;
  serde_json::from_str(&data).ok()
}

pub fn clear(client_id: &str) {
  if let Some(path) = store_path(client_id) {
    let _ = fs::remove_file(path);
  }
}

/// Client ids that currently have a stored grant, sorted. Reads only file
/// names; a name that is not a valid application id is skipped, not an error.
#[must_use]
pub fn list() -> Vec<String> {
  let Some(dir) = store_dir() else {
    return Vec::new();
  };
  let Ok(entries) = fs::read_dir(dir) else {
    return Vec::new();
  };
  let mut ids: Vec<String> = entries
    .filter_map(|entry| entry.ok())
    .map(|entry| entry.file_name().to_string_lossy().to_string())
    .filter_map(|name| name.strip_suffix(".json").map(str::to_owned))
    .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    .collect();
  ids.sort();
  ids
}

/// File mode of the stored grant for `client_id`, when the file exists.
/// Used at startup to surface grants left with looser permissions by an
/// older Adsum version.
#[cfg(unix)]
#[must_use]
pub fn stored_mode(client_id: &str) -> Option<u32> {
  use std::os::unix::fs::PermissionsExt;
  let path = store_path(client_id)?;
  let metadata = fs::metadata(&path).ok()?;
  Some(metadata.permissions().mode() & 0o777)
}

pub fn client_secret() -> Option<String> {
  std::env::var("ADSUM_OAUTH_CLIENT_SECRET")
    .ok()
    .filter(|s| !s.is_empty())
}

#[cfg(test)]
#[path = "../tests/unit/store.rs"]
mod tests;
