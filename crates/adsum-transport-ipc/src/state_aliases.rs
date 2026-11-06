//! Symlink aliases that publish the canonical state snapshot into client
//! runtime directories, so clients that scan their own temp dir (Equibop
//! Flatpak: `os.tmpdir()/arrpc-state-N`) find it.
//!
//! Unlike the IPC alias markers, the snapshot target is rewritten
//! atomically (temp + rename), so its inode changes on every write and is
//! never used for validation. The marker records the symlink identity and
//! the target path; the target itself is validated as a regular,
//! owner-only, parseable snapshot file. Ownership, marker-first
//! publication, exclusive creation and conflict-preserving removal follow
//! the `markers` module principles.

use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::markers::remove_if_identity;
use crate::paths::{socket_alias_dirs, validate_alias_dir};
use crate::probe::FileId;

/// Marker schema version for state-snapshot aliases.
pub(crate) const STATE_MARKER_VERSION: u32 = 1;

/// Maximum marker file size in bytes (matches IPC markers).
pub(crate) const STATE_MARKER_MAX_BYTES: u64 = 4096;

/// Maximum canonical snapshot size: the file embeds the activity list, so
/// the cap is well above the largest realistic snapshot.
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
  /// Marker path that was validated.
  path: PathBuf,
  /// `(dev, ino)` of the validated marker.
  identity: FileId,
}

/// An alias and marker created by this daemon for one snapshot slot.
#[derive(Debug)]
pub struct OwnedStateAlias {
  /// Snapshot file name the alias publishes (e.g. `arrpc-state-0`).
  pub name: String,
  /// Alias symlink path inside the client runtime directory.
  pub alias: PathBuf,
  /// `(dev, ino)` of the alias symlink at creation.
  pub alias_id: FileId,
  /// Marker path published before the alias.
  pub marker: PathBuf,
  /// `(dev, ino)` of the validated marker.
  pub marker_id: FileId,
  /// Canonical snapshot path the alias points at.
  pub target: PathBuf,
}

/// Outcome of one publication attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatePublish {
  /// The alias was created (or already pointed at the canonical snapshot).
  Linked,
  /// An existing entry could not be claimed; nothing was touched.
  Skipped,
}

/// Outcome of one recovery pass over one alias/marker pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRecovery {
  /// Nothing to do: no marker and no alias.
  Absent,
  /// A missing alias was re-created from a valid marker (healed).
  Healed,
  /// The pair is owned and healthy; left untouched.
  Preserved,
  /// A dead pair (missing or stale target) was removed.
  Cleaned,
  /// Foreign, divergent or inconclusive: preserved, never touched.
  Conflict,
}

/// Marker file name for one alias name and owner.
fn state_marker_name(euid: u32, name: &str) -> String {
  format!(".adsum-state-owner-{euid}-{name}")
}

/// Marker path inside `dir`.
fn state_marker_path(dir: &Path, euid: u32, name: &str) -> PathBuf {
  dir.join(state_marker_name(euid, name))
}

/// Validate the canonical snapshot target: a regular file (never a symlink),
/// owned by `euid`, owner-only permissions, and parseable JSON with a
/// `timestamp`. The target inode is deliberately not recorded anywhere.
fn validate_snapshot_target(target: &Path, euid: u32) -> io::Result<()> {
  let meta = std::fs::metadata(target)?;
  if !meta.is_file() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "snapshot target is not a regular file",
    ));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
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
  }
  let content = read_bounded(target, SNAPSHOT_MAX_BYTES)?;
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

/// Read at most `max` bytes; larger files are rejected, not truncated.
fn read_bounded(path: &Path, max: u64) -> io::Result<String> {
  let meta = std::fs::metadata(path)?;
  if meta.len() > max {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "file exceeds the size cap",
    ));
  }
  std::fs::read_to_string(path)
}

/// Open the marker without following symlinks, exclusive, 0600.
fn open_state_marker(path: &Path) -> io::Result<std::fs::File> {
  use std::os::unix::fs::OpenOptionsExt;
  std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .mode(0o600)
    .custom_flags(libc::O_NOFOLLOW)
    .open(path)
}

/// Read and validate a marker file: no symlinks, size cap, owner, schema.
fn read_state_marker(path: &Path, euid: u32) -> io::Result<(FileId, StateMarkerData)> {
  use std::os::unix::fs::MetadataExt;

  let meta = std::fs::symlink_metadata(path)?;
  if meta.file_type().is_symlink() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "marker is a symlink",
    ));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != euid {
      return Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "marker is owned by another uid",
      ));
    }
  }
  if meta.len() > STATE_MARKER_MAX_BYTES {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "marker exceeds the size cap",
    ));
  }
  let content = std::fs::read_to_string(path)?;
  let data: StateMarkerData = serde_json::from_str(&content)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
  if data.version != STATE_MARKER_VERSION {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "unknown marker version",
    ));
  }
  Ok((
    FileId {
      dev: meta.dev(),
      ino: meta.ino(),
    },
    data,
  ))
}

/// `(dev, ino)` of `alias` when it is a symlink pointing exactly at
/// `target`, stable across two reads.
fn state_symlink_id(alias: &Path, target: &Path) -> Option<FileId> {
  use std::os::unix::fs::MetadataExt;

  let first = std::fs::symlink_metadata(alias).ok()?;
  if !first.file_type().is_symlink() {
    return None;
  }
  let identity = FileId {
    dev: first.dev(),
    ino: first.ino(),
  };
  if std::fs::read_link(alias).ok()? != target {
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

/// Publish the state alias for `name` pointing at `target`, in `dir`.
///
/// The marker is written first (exclusive, never overwriting), then the
/// symlink; a failure between the two removes the marker. An existing
/// symlink that already points at the canonical target under a valid marker
/// reports [`StatePublish::Linked`] instead of being recreated.
pub fn publish_state_alias(dir: &Path, name: &str, target: &Path, euid: u32) -> StatePublish {
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
  let alias = dir.join(name);
  let marker_path = state_marker_path(dir, euid, name);
  if let Ok((_, data)) = read_state_marker(&marker_path, euid) {
    // Marker exists: only heal when it provably describes this alias.
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
  let mut marker_file = match open_state_marker(&marker_path) {
    Ok(file) => file,
    Err(err) => {
      tracing::debug!(
        "[state-alias] Marker creation failed for {}: {err}",
        alias.display()
      );
      return StatePublish::Skipped;
    }
  };
  if let Err(err) = std::io::Write::write_all(&mut marker_file, &payload) {
    tracing::debug!(
      "[state-alias] Marker write failed for {}: {err}",
      alias.display()
    );
    let _ = std::fs::remove_file(&marker_path);
    return StatePublish::Skipped;
  }
  if let Err(err) = std::os::unix::fs::symlink(target, &alias) {
    tracing::debug!(
      "[state-alias] Symlink creation failed for {}: {err}",
      alias.display()
    );
    let _ = std::fs::remove_file(&marker_path);
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
      let _ = std::fs::remove_file(&marker_path);
      StatePublish::Skipped
    }
  }
}

/// Load and revalidate the marker for one alias pair against the expected
/// alias and target paths. The alias symlink itself is not required here:
/// a missing alias over a valid marker is exactly the heal case.
fn revalidate_state_marker(
  dir: &Path,
  name: &str,
  target: &Path,
  euid: u32,
) -> Result<ValidStateMarker, StateRecovery> {
  let marker_path = state_marker_path(dir, euid, name);
  let alias = dir.join(name);
  let (identity, data) =
    read_state_marker(&marker_path, euid).map_err(|_| StateRecovery::Conflict)?;
  if Path::new(&data.alias) != alias || Path::new(&data.target) != target {
    return Err(StateRecovery::Conflict);
  }
  Ok(ValidStateMarker {
    path: marker_path,
    identity,
  })
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

/// Recover one alias/marker pair, then report the resulting state.
///
/// Foreign and inconclusive entries are preserved. A missing alias over a
/// valid marker and live target is healed; a dead or stale pair is cleaned.
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
  if read_state_marker(&marker_path, euid).is_err() {
    return match std::fs::symlink_metadata(&alias) {
      Err(err) if err.kind() == io::ErrorKind::NotFound => StateRecovery::Absent,
      _ => StateRecovery::Preserved,
    };
  }
  let marker = match revalidate_state_marker(dir, name, target, euid) {
    Ok(marker) => marker,
    Err(StateRecovery::Conflict) => return StateRecovery::Conflict,
    Err(_) => return StateRecovery::Preserved,
  };
  // Heal when the marker is valid but the alias symlink is missing or no
  // longer points at the canonical snapshot.
  if state_symlink_id(&alias, target).is_none() {
    if validate_snapshot_target(target, euid).is_ok() {
      if std::os::unix::fs::symlink(target, &alias).is_ok()
        && state_symlink_id(&alias, target).is_some()
      {
        return StateRecovery::Healed;
      }
      return StateRecovery::Preserved;
    }
    remove_state_pair(&alias, &marker);
    return StateRecovery::Cleaned;
  }
  // Healthy alias: a live daemon owns the pair unless the target went
  // missing or stale (beyond the consumer budget) — then clean up.
  let age = std::fs::metadata(target)
    .and_then(|meta| meta.modified())
    .ok()
    .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
    .map(|age| now_ms.saturating_sub(age.as_millis() as u64));
  match age {
    Some(age) if age <= consumer_stale_ms => StateRecovery::Preserved,
    _ => {
      remove_state_pair(&alias, &marker);
      StateRecovery::Cleaned
    }
  }
}

/// Client runtime directories that exist and validate, in publish order.
#[must_use]
pub fn state_alias_dirs(bases: &[PathBuf]) -> Vec<PathBuf> {
  socket_alias_dirs(bases)
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
      match publish_state_alias(dir, name, target, euid) {
        StatePublish::Linked => {
          let alias_id = state_symlink_id(&alias, target);
          if let (Some(alias_id), Ok((marker_id, _))) = (alias_id, read_state_marker(&marker, euid))
          {
            owned.push(OwnedStateAlias {
              name: name.to_owned(),
              alias,
              alias_id,
              marker,
              marker_id,
              target: target.to_path_buf(),
            });
            StateRecovery::Healed
          } else {
            StateRecovery::Conflict
          }
        }
        StatePublish::Skipped => StateRecovery::Conflict,
      }
    }
    StateRecovery::Preserved => {
      // Healthy pair: record ownership so shutdown can remove it.
      if !owned.iter().any(|entry| entry.alias == alias)
        && let (Some(alias_id), Ok((marker_id, _))) = (
          state_symlink_id(&alias, target),
          read_state_marker(&marker, euid),
        )
      {
        owned.push(OwnedStateAlias {
          name: name.to_owned(),
          alias,
          alias_id,
          marker,
          marker_id,
          target: target.to_path_buf(),
        });
      }
      StateRecovery::Preserved
    }
    StateRecovery::Healed | StateRecovery::Conflict => StateRecovery::Conflict,
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
  if state_symlink_id(&owned.alias, &owned.target).is_none() {
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

#[cfg(all(test, unix))]
#[path = "../tests/unit/state_aliases.rs"]
mod tests;
