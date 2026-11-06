//! Symlink aliases that publish the canonical state snapshot into client
//! runtime directories. Ownership, marker-first publication, exclusive
//! creation and conflict-preserving removal follow the `markers` principles.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::markers::remove_if_identity;
use crate::paths::{socket_alias_dirs, validate_alias_dir};
use crate::probe::FileId;

/// Marker schema version for state-snapshot aliases.
pub(crate) const STATE_MARKER_VERSION: u32 = 1;

/// Maximum marker file size in bytes (matches IPC markers).
pub(crate) const STATE_MARKER_MAX_BYTES: u64 = 4096;

/// Maximum canonical snapshot size: the file embeds the activity list.
pub const SNAPSHOT_MAX_BYTES: u64 = 64 * 1024;

/// Default consumer staleness budget (Equibop treats older state as stale).
pub const STATE_CONSUMER_STALE_MS: u64 = 60_000;

/// Versioned content of a state-alias ownership marker.
#[derive(Debug, Serialize, Deserialize)]
struct StateMarkerData {
  version: u32,
  owner_uid: u32,
  alias: String,
  target: String,
}

/// A validated state marker: path and opened identity.
#[derive(Debug)]
struct ValidStateMarker {
  path: PathBuf,
  identity: FileId,
}

/// An alias and marker created by this daemon for one snapshot slot.
#[derive(Debug)]
pub struct OwnedStateAlias {
  pub name: String,
  pub alias: PathBuf,
  pub alias_id: FileId,
  pub marker: PathBuf,
  pub marker_id: FileId,
  pub target: PathBuf,
}

/// Outcome of one publication attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatePublish {
  Linked,
  Skipped,
}

/// Outcome of one recovery pass over one alias and marker pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRecovery {
  Absent,
  Healed,
  Preserved,
  Cleaned,
  Conflict,
}

/// Ownership class of the entry currently sitting at an alias path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AliasState {
  Missing,
  Ours(FileId),
  Foreign,
}

/// Marker file name for one alias name and owner.
fn state_marker_name(euid: u32, name: &str) -> String {
  format!(".adsum-state-owner-{euid}-{name}")
}

/// Marker path inside `dir`.
fn state_marker_path(dir: &Path, euid: u32, name: &str) -> PathBuf {
  dir.join(state_marker_name(euid, name))
}

/// Require a regular, euid-owned, 0600 entry no larger than `max`.
fn check_state_marker_meta(meta: &std::fs::Metadata, euid: u32, max: u64) -> io::Result<()> {
  if !meta.file_type().is_file() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "state marker is not a regular file",
    ));
  }
  if meta.uid() != euid {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "state marker is not owned by the effective uid",
    ));
  }
  if meta.mode() & 0o7777 != 0o600 {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "state marker mode is not 0600",
    ));
  }
  if meta.len() > max {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "state marker exceeds the size cap",
    ));
  }
  Ok(())
}

/// Read and validate a marker: regular file, 0600, opened `O_NOFOLLOW`,
/// identity stable across the read, schema and owner checked.
fn read_state_marker(path: &Path, euid: u32) -> io::Result<(FileId, StateMarkerData)> {
  let meta = std::fs::symlink_metadata(path)?;
  check_state_marker_meta(&meta, euid, STATE_MARKER_MAX_BYTES)?;

  let file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
    .open(path)?;
  let opened = file.metadata()?;
  check_state_marker_meta(&opened, euid, STATE_MARKER_MAX_BYTES)?;
  let identity = FileId {
    dev: opened.dev(),
    ino: opened.ino(),
  };

  let mut content = Vec::with_capacity(STATE_MARKER_MAX_BYTES as usize + 1);
  file
    .take(STATE_MARKER_MAX_BYTES + 1)
    .read_to_end(&mut content)?;
  if content.len() as u64 > STATE_MARKER_MAX_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "state marker exceeds the size cap",
    ));
  }
  let data: StateMarkerData = serde_json::from_slice(&content)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
  if data.version != STATE_MARKER_VERSION {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "unknown state marker version",
    ));
  }
  if data.owner_uid != euid {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "state marker owner_uid does not match the effective uid",
    ));
  }
  let after = std::fs::symlink_metadata(path)?;
  if after.dev() != identity.dev || after.ino() != identity.ino {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "state marker changed during validation",
    ));
  }
  Ok((identity, data))
}

/// Open the canonical snapshot without following symlinks, then validate
/// and read it through the same descriptor: type, uid, 0600, size cap.
fn validate_snapshot_target(target: &Path, euid: u32) -> io::Result<()> {
  let file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
    .open(target)?;
  let meta = file.metadata()?;
  if !meta.is_file() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "snapshot target is not a regular file",
    ));
  }
  if meta.uid() != euid {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "snapshot target is owned by another uid",
    ));
  }
  if meta.mode() & 0o777 != 0o600 {
    return Err(io::Error::new(
      io::ErrorKind::PermissionDenied,
      "snapshot target permissions are not 0600",
    ));
  }
  if meta.len() > SNAPSHOT_MAX_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "snapshot target exceeds the size cap",
    ));
  }
  let mut content = String::new();
  file
    .take(SNAPSHOT_MAX_BYTES + 1)
    .read_to_string(&mut content)?;
  if content.len() as u64 > SNAPSHOT_MAX_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "snapshot target exceeds the size cap",
    ));
  }
  let snapshot: serde_json::Value = serde_json::from_str(&content)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
  if snapshot
    .get("timestamp")
    .and_then(serde_json::Value::as_i64)
    .is_none()
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "snapshot target has no timestamp",
    ));
  }
  Ok(())
}

/// Age of the snapshot in milliseconds; `None` when unreadable.
fn snapshot_target_age(target: &Path, now_ms: u64) -> Option<u64> {
  std::fs::metadata(target)
    .and_then(|meta| meta.modified())
    .ok()
    .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
    .and_then(|age| u64::try_from(age.as_millis()).ok())
    .map(|age| now_ms.saturating_sub(age))
}

/// `(dev, ino)` of `alias` when it is a symlink to `target`, stable across reads.
fn state_symlink_id(alias: &Path, target: &Path) -> Option<FileId> {
  let first = std::fs::symlink_metadata(alias).ok()?;
  if !first.file_type().is_symlink() {
    return None;
  }
  let identity = FileId {
    dev: first.dev(),
    ino: first.ino(),
  };
  if std::fs::read_link(alias).ok()?.as_path() != target {
    return None;
  }
  let second = std::fs::symlink_metadata(alias).ok()?;
  if !second.file_type().is_symlink()
    || second.dev() != identity.dev
    || second.ino() != identity.ino
  {
    return None;
  }
  Some(identity)
}

/// Classify the alias path: gone, provably ours, or anything else.
fn classify_alias(alias: &Path, target: &Path) -> AliasState {
  match std::fs::symlink_metadata(alias) {
    Err(err) if err.kind() == io::ErrorKind::NotFound => AliasState::Missing,
    _ => match state_symlink_id(alias, target) {
      Some(id) => AliasState::Ours(id),
      None => AliasState::Foreign,
    },
  }
}

/// Unique temporary marker path beside the final one.
fn temp_state_marker_path(marker_path: &Path) -> PathBuf {
  static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
  let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
  let pid = std::process::id();
  marker_path.with_extension(format!("tmp-{pid}-{counter}"))
}

/// Fill the temp marker, sync it, and link it under the final name.
fn write_and_link_state_marker(
  mut file: File,
  temp_path: &Path,
  final_path: &Path,
  content: &[u8],
) -> io::Result<()> {
  file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
  file.write_all(content)?;
  file.sync_all()?;
  drop(file);
  std::fs::hard_link(temp_path, final_path)
}

/// Publish the marker atomically: temp write, sync, hard link, temp cleanup.
/// A crash leaves an orphan temp, never a partial final marker.
fn publish_state_marker(marker_path: &Path, content: &[u8], euid: u32) -> io::Result<FileId> {
  let mut last_err = None;
  for _ in 0..8 {
    let temp_path = temp_state_marker_path(marker_path);
    let file = match OpenOptions::new()
      .write(true)
      .create_new(true)
      .mode(0o600)
      .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
      .open(&temp_path)
    {
      Ok(file) => file,
      Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
        last_err = Some(err);
        continue;
      }
      Err(err) => return Err(err),
    };
    let meta = file.metadata()?;
    let identity = FileId {
      dev: meta.dev(),
      ino: meta.ino(),
    };
    if let Err(err) = write_and_link_state_marker(file, &temp_path, marker_path, content) {
      let _ = remove_if_identity(&temp_path, identity);
      return Err(err);
    }
    let _ = remove_if_identity(&temp_path, identity);

    let published = std::fs::symlink_metadata(marker_path)?;
    check_state_marker_meta(&published, euid, STATE_MARKER_MAX_BYTES)?;
    if published.dev() != identity.dev || published.ino() != identity.ino {
      return Err(io::Error::other("published state marker identity changed"));
    }
    return Ok(identity);
  }
  Err(last_err.unwrap_or_else(|| {
    io::Error::new(
      io::ErrorKind::AlreadyExists,
      "temporary marker name exhausted",
    )
  }))
}

/// Publish the state alias for `name` pointing at `target` in `dir`.
///
/// The marker is published atomically before the symlink; a failure between
/// the two removes the marker. A healthy existing pair reports
/// [`StatePublish::Linked`]; a target older than `consumer_stale_ms` is
/// refused so publication never fights the staleness cleanup.
pub fn publish_state_alias(
  dir: &Path,
  name: &str,
  target: &Path,
  euid: u32,
  consumer_stale_ms: u64,
  now_ms: u64,
) -> StatePublish {
  if let Err(err) = validate_snapshot_target(target, euid) {
    tracing::debug!(
      "[state-alias] Target {} rejected as a snapshot: {err}",
      target.display()
    );
    return StatePublish::Skipped;
  }
  if let Err(err) = validate_alias_dir(dir) {
    tracing::debug!("[state-alias] Dir {} rejected: {err}", dir.display());
    return StatePublish::Skipped;
  }
  if snapshot_target_age(target, now_ms).is_some_and(|age| age > consumer_stale_ms) {
    tracing::debug!(
      "[state-alias] Target {} is older than the staleness budget; not republished",
      target.display()
    );
    return StatePublish::Skipped;
  }
  let alias = dir.join(name);
  let marker_path = state_marker_path(dir, euid, name);
  if let Ok((_, data)) = read_state_marker(&marker_path, euid) {
    // A valid marker exists: heal only when it provably describes this alias.
    if Path::new(&data.alias) == alias
      && Path::new(&data.target) == target
      && state_symlink_id(&alias, target).is_some()
    {
      return StatePublish::Linked;
    }
    tracing::debug!(
      "[state-alias] Marker {} diverges; skipping {}",
      marker_path.display(),
      alias.display()
    );
    return StatePublish::Skipped;
  }
  if std::fs::symlink_metadata(&alias).is_ok() {
    tracing::debug!(
      "[state-alias] Alias {} exists without our marker; preserving it",
      alias.display()
    );
    return StatePublish::Skipped;
  }
  let payload = serde_json::to_vec(&StateMarkerData {
    version: STATE_MARKER_VERSION,
    owner_uid: euid,
    alias: alias.to_string_lossy().into_owned(),
    target: target.to_string_lossy().into_owned(),
  })
  .expect("state marker serializes");
  let identity = match publish_state_marker(&marker_path, &payload, euid) {
    Ok(identity) => identity,
    Err(err) => {
      tracing::debug!(
        "[state-alias] Marker publication failed for {}: {err}",
        alias.display()
      );
      return StatePublish::Skipped;
    }
  };
  if let Err(err) = std::os::unix::fs::symlink(target, &alias) {
    tracing::debug!(
      "[state-alias] Symlink creation failed for {}: {err}",
      alias.display()
    );
    let _ = remove_if_identity(&marker_path, identity);
    return StatePublish::Skipped;
  }
  match state_symlink_id(&alias, target) {
    Some(_) => {
      tracing::debug!(
        "[state-alias] Published {} -> {}",
        alias.display(),
        target.display()
      );
      StatePublish::Linked
    }
    None => {
      tracing::debug!(
        "[state-alias] Alias {} did not verify after creation",
        alias.display()
      );
      let _ = std::fs::remove_file(&alias);
      let _ = remove_if_identity(&marker_path, identity);
      StatePublish::Skipped
    }
  }
}

/// Remove a validated alias and then its marker, rechecking both.
fn remove_state_pair(alias: &Path, marker: &ValidStateMarker) {
  if std::fs::remove_file(alias).is_err() {
    tracing::debug!(
      "[state-alias] Alias removal failed for {}; keeping the marker",
      alias.display()
    );
    return;
  }
  let _ = remove_if_identity(&marker.path, marker.identity);
}

/// Recover one alias and marker pair, then report the resulting state.
/// Only provably owned entries are touched: anything foreign, divergent
/// or inconclusive is preserved.
pub fn recover_state_alias(
  dir: &Path,
  name: &str,
  target: &Path,
  euid: u32,
  consumer_stale_ms: u64,
  now_ms: u64,
) -> StateRecovery {
  let marker_path = state_marker_path(dir, euid, name);
  let alias = dir.join(name);
  // Without a valid marker nothing there is provably ours: preserve.
  let (marker_id, data) = match read_state_marker(&marker_path, euid) {
    Ok(found) => found,
    Err(_) => {
      return match std::fs::symlink_metadata(&alias) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => StateRecovery::Absent,
        _ => StateRecovery::Preserved,
      };
    }
  };
  if Path::new(&data.alias) != alias || Path::new(&data.target) != target {
    return StateRecovery::Conflict;
  }
  let marker = ValidStateMarker {
    path: marker_path,
    identity: marker_id,
  };
  match classify_alias(&alias, target) {
    // A renamed, replaced or foreign entry is never ours to remove.
    AliasState::Foreign => StateRecovery::Conflict,
    AliasState::Missing => {
      if validate_snapshot_target(target, euid).is_ok() {
        if std::os::unix::fs::symlink(target, &alias).is_ok()
          && state_symlink_id(&alias, target).is_some()
        {
          return StateRecovery::Healed;
        }
        return StateRecovery::Preserved;
      }
      // The target is gone and the alias is gone: only our own orphan
      // marker remains, and its recorded identity authorizes removal.
      if remove_if_identity(&marker.path, marker.identity) {
        StateRecovery::Cleaned
      } else {
        StateRecovery::Conflict
      }
    }
    AliasState::Ours(_) => {
      let stale = snapshot_target_age(target, now_ms).is_none_or(|age| age > consumer_stale_ms);
      if !stale {
        return StateRecovery::Preserved;
      }
      // Provably ours, verified twice: the narrow unlink window matches
      // the IPC marker recovery contract.
      if std::fs::remove_file(&alias).is_ok() {
        let _ = remove_if_identity(&marker.path, marker.identity);
        StateRecovery::Cleaned
      } else {
        StateRecovery::Conflict
      }
    }
  }
}

/// Client runtime directories that exist and validate, in publish order.
#[must_use]
pub fn state_alias_dirs(bases: &[PathBuf]) -> Vec<PathBuf> {
  socket_alias_dirs(bases)
}

/// Record ownership of a healthy pair, refreshing any stale record.
fn record_state_ownership(
  owned: &mut Vec<OwnedStateAlias>,
  name: &str,
  alias: &Path,
  target: &Path,
  marker: &Path,
  euid: u32,
) -> bool {
  if let (Some(alias_id), Ok((marker_id, _))) = (
    state_symlink_id(alias, target),
    read_state_marker(marker, euid),
  ) {
    owned.retain(|entry| entry.alias != alias);
    owned.push(OwnedStateAlias {
      name: name.to_owned(),
      alias: alias.to_path_buf(),
      alias_id,
      marker: marker.to_path_buf(),
      marker_id,
      target: target.to_path_buf(),
    });
    true
  } else {
    false
  }
}

/// Ensure one alias exists for `name` -> `target` in `dir`, healing or
/// cleaning per [`recover_state_alias`], and record ownership in `owned`.
pub fn reconcile_state_alias(
  dir: &Path,
  name: &str,
  target: &Path,
  euid: u32,
  owned: &mut Vec<OwnedStateAlias>,
  consumer_stale_ms: u64,
  now_ms: u64,
) -> StateRecovery {
  let alias = dir.join(name);
  let marker = state_marker_path(dir, euid, name);
  match recover_state_alias(dir, name, target, euid, consumer_stale_ms, now_ms) {
    StateRecovery::Absent | StateRecovery::Cleaned => {
      owned.retain(|entry| entry.alias != alias);
      match publish_state_alias(dir, name, target, euid, consumer_stale_ms, now_ms) {
        StatePublish::Linked => {
          if record_state_ownership(owned, name, &alias, target, &marker, euid) {
            StateRecovery::Healed
          } else {
            StateRecovery::Conflict
          }
        }
        StatePublish::Skipped => StateRecovery::Conflict,
      }
    }
    // The recovery pass recreated the alias: refresh its ownership record
    // so the shutdown removal finds it under the fresh inode.
    StateRecovery::Healed => {
      if record_state_ownership(owned, name, &alias, target, &marker, euid) {
        StateRecovery::Healed
      } else {
        StateRecovery::Conflict
      }
    }
    StateRecovery::Preserved => {
      if !owned.iter().any(|entry| entry.alias == alias) {
        let _ = record_state_ownership(owned, name, &alias, target, &marker, euid);
      }
      StateRecovery::Preserved
    }
    StateRecovery::Conflict => StateRecovery::Conflict,
  }
}

/// Remove one owned alias then its marker, revalidating every recorded
/// identity and the exact target path. Anything divergent is preserved.
pub fn remove_owned_state_alias(owned: &OwnedStateAlias, euid: u32) {
  let marker_path = state_marker_path(
    owned.alias.parent().unwrap_or_else(|| Path::new("/")),
    euid,
    &owned.name,
  );
  let Ok((marker_id, data)) = read_state_marker(&marker_path, euid) else {
    tracing::debug!(
      "[state-alias] Marker {} missing or invalid; preserving alias",
      marker_path.display()
    );
    return;
  };
  if marker_id != owned.marker_id
    || Path::new(&data.alias) != owned.alias.as_path()
    || Path::new(&data.target) != owned.target.as_path()
  {
    tracing::debug!(
      "[state-alias] Marker {} diverges from the ownership record; preserving",
      marker_path.display()
    );
    return;
  }
  if state_symlink_id(&owned.alias, &owned.target) != Some(owned.alias_id) {
    tracing::debug!(
      "[state-alias] Alias {} changed since publication; preserving it",
      owned.alias.display()
    );
    return;
  }
  let marker = ValidStateMarker {
    path: marker_path,
    identity: marker_id,
  };
  remove_state_pair(&owned.alias, &marker);
}

/// Remove every owned alias, draining the vector in order.
pub fn remove_owned_state_aliases(owned: &mut Vec<OwnedStateAlias>, euid: u32) {
  for alias in owned.drain(..) {
    remove_owned_state_alias(&alias, euid);
  }
}

#[cfg(test)]
#[path = "../tests/unit/state_aliases.rs"]
mod tests;
