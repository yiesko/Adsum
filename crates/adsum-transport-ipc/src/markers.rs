//! Alias ownership markers: schema, validation, and atomic publication.
//!
//! A marker is never overwritten and never authorizes removing an alias
//! unless its recorded `(dev, ino)` still matches the on-disk file.

use std::ffi::OsStr;
use std::fs::{File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::paths::validate_alias_dir;
use crate::probe::{FileId, SocketState, probe_socket};

/// Current marker schema version.
pub(crate) const MARKER_VERSION: u32 = 1;

/// Maximum marker file size in bytes.
pub(crate) const MAX_MARKER_BYTES: u64 = 4096;

/// Versioned JSON content of an alias ownership marker.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MarkerData {
  /// Schema version; must equal [`MARKER_VERSION`].
  pub version: u32,
  /// Effective uid that published the marker.
  pub owner_uid: u32,
  /// Absolute alias path created by the owner.
  pub alias: String,
  /// Absolute bound-socket path the alias points at.
  pub target: String,
  /// Device of the bound socket when the marker was published.
  pub target_dev: u64,
  /// Inode of the bound socket when the marker was published.
  pub target_ino: u64,
}

/// A marker whose identity and content were validated.
#[derive(Debug)]
pub(crate) struct ValidMarker {
  /// Marker path that was validated.
  pub path: PathBuf,
  /// `(dev, ino)` of the validated marker.
  pub identity: FileId,
  /// Parsed marker content.
  pub data: MarkerData,
}

/// Result of inspecting one marker path.
#[derive(Debug)]
pub(crate) enum MarkerCheck {
  /// Nothing exists at the marker path.
  Missing,
  /// The marker is valid and its identity is recorded.
  Valid(ValidMarker),
  /// The entry is invalid, foreign, or changed during validation.
  Conflict(String),
}

/// An alias and marker created by this transport, with both identities.
#[derive(Debug)]
pub(crate) struct OwnedAlias {
  /// Socket index the alias belongs to.
  pub index: u8,
  /// Alias symlink path.
  pub alias: PathBuf,
  /// `(dev, ino)` of the alias symlink when it was created.
  pub alias_id: FileId,
  /// Validated marker published before the alias.
  pub marker: ValidMarker,
  /// Bound socket path the alias points at.
  pub target: PathBuf,
  /// `(dev, ino)` of the bound socket when the marker was published.
  pub target_id: FileId,
}

/// Result of inspecting one alias and marker pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
  /// The alias path is free.
  Absent,
  /// A live or foreign entry was left untouched.
  Preserved,
  /// The directory, marker, or alias is invalid or divergent.
  Conflict,
}

/// Result of publishing one alias.
#[derive(Debug)]
pub(crate) enum PublishOutcome {
  /// The alias was created and is owned by this transport.
  Created(OwnedAlias),
  /// An existing entry, conflict, or failure prevented creation.
  Skipped,
}

/// Marker file name for `euid` and `index`.
#[must_use]
pub(crate) fn marker_name(euid: u32, index: u8) -> String {
  format!(".adsum-ipc-owner-{euid}-{index}")
}

/// Marker path inside `alias_dir`.
#[must_use]
pub(crate) fn marker_path(alias_dir: &Path, euid: u32, index: u8) -> PathBuf {
  alias_dir.join(marker_name(euid, index))
}

/// Validate the marker at `path` for `expected_alias`; never unlinks anything.
#[must_use]
pub(crate) fn validate_marker(
  path: &Path,
  expected_alias: &Path,
  euid: u32,
  index: u8,
) -> MarkerCheck {
  let meta = match std::fs::symlink_metadata(path) {
    Ok(meta) => meta,
    Err(err) if err.kind() == io::ErrorKind::NotFound => return MarkerCheck::Missing,
    Err(err) => return MarkerCheck::Conflict(format!("marker lstat failed: {err}")),
  };
  match validate_existing_marker(path, &meta, expected_alias, euid, index) {
    Ok(marker) => MarkerCheck::Valid(marker),
    Err(reason) => MarkerCheck::Conflict(reason),
  }
}

/// Publish a marker without replacing an existing one, returning its identity.
pub(crate) fn publish_marker(
  alias_dir: &Path,
  alias: &Path,
  target: &Path,
  target_id: FileId,
  euid: u32,
  index: u8,
) -> io::Result<ValidMarker> {
  let alias_str = alias
    .to_str()
    .ok_or_else(|| invalid_input("alias is not valid UTF-8"))?;
  let target_str = target
    .to_str()
    .ok_or_else(|| invalid_input("target is not valid UTF-8"))?;
  if !alias.is_absolute() || !target.is_absolute() {
    return Err(invalid_input("alias and target must be absolute"));
  }
  if alias.parent() != Some(alias_dir) {
    return Err(invalid_input(
      "alias parent does not match the alias directory",
    ));
  }
  let expected = socket_name(index);
  if alias.file_name() != Some(OsStr::new(&expected))
    || target.file_name() != Some(OsStr::new(&expected))
  {
    return Err(invalid_input("alias and target names must match the index"));
  }

  let data = MarkerData {
    version: MARKER_VERSION,
    owner_uid: euid,
    alias: alias_str.to_owned(),
    target: target_str.to_owned(),
    target_dev: target_id.dev,
    target_ino: target_id.ino,
  };
  let content = serde_json::to_vec(&data).map_err(io::Error::other)?;
  if content.len() as u64 > MAX_MARKER_BYTES {
    return Err(invalid_input("marker content exceeds 4096 bytes"));
  }

  let final_path = marker_path(alias_dir, euid, index);
  let (file, temp_path) = create_temp_marker(|| {
    let temp_path = temp_marker_path(alias_dir, euid, index);
    let file = OpenOptions::new()
      .write(true)
      .create_new(true)
      .mode(0o600)
      .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
      .open(&temp_path)?;
    Ok((file, temp_path))
  })?;
  let identity = {
    let meta = file.metadata()?;
    FileId {
      dev: meta.dev(),
      ino: meta.ino(),
    }
  };

  if let Err(err) = write_and_link(file, &temp_path, &final_path, &content) {
    let _ = remove_if_identity(&temp_path, identity);
    return Err(err);
  }
  // The temp name shares the inode with the final marker; removing it after
  // publication is safe and leaves the final entry intact.
  let _ = remove_if_identity(&temp_path, identity);

  let meta = std::fs::symlink_metadata(&final_path)?;
  check_marker_meta(&meta, euid).map_err(io::Error::other)?;
  let final_identity = FileId {
    dev: meta.dev(),
    ino: meta.ino(),
  };
  if final_identity != identity {
    return Err(io::Error::other("published marker identity changed"));
  }
  Ok(ValidMarker {
    path: final_path,
    identity: final_identity,
    data,
  })
}

/// Remove `marker` only when its identity still matches; true on removal.
#[must_use]
pub(crate) fn remove_marker_if_unchanged(marker: &ValidMarker) -> bool {
  remove_if_identity(&marker.path, marker.identity)
}

/// Recover one alias/marker pair, removing only validated dead orphans.
#[must_use]
pub(crate) fn recover_alias(alias_dir: &Path, index: u8, euid: u32) -> Recovery {
  if let Err(err) = validate_alias_dir(alias_dir) {
    tracing::debug!(
      "[ipc] Alias dir {} rejected during recovery: {err}",
      alias_dir.display()
    );
    return Recovery::Conflict;
  }
  let alias = alias_dir.join(socket_name(index));
  let marker_file = marker_path(alias_dir, euid, index);
  match validate_marker(&marker_file, &alias, euid, index) {
    MarkerCheck::Missing => match std::fs::symlink_metadata(&alias) {
      Err(err) if err.kind() == io::ErrorKind::NotFound => Recovery::Absent,
      Err(err) => {
        tracing::debug!(
          "[ipc] Alias {} could not be inspected: {err}",
          alias.display()
        );
        Recovery::Preserved
      }
      Ok(_) => Recovery::Preserved,
    },
    MarkerCheck::Conflict(reason) => {
      tracing::debug!(
        "[ipc] Marker conflict at {}: {reason}",
        marker_file.display()
      );
      Recovery::Conflict
    }
    MarkerCheck::Valid(marker) => recover_valid_marker(&alias, &marker),
  }
}

/// Recover every known alias index in each validated alias directory.
pub(crate) fn recover_orphan_aliases(alias_dirs: &[PathBuf], euid: u32) {
  for alias_dir in alias_dirs {
    if let Err(err) = validate_alias_dir(alias_dir) {
      tracing::debug!(
        "[ipc] Alias dir {} rejected during recovery: {err}",
        alias_dir.display()
      );
      continue;
    }
    for index in 0..=9 {
      if recover_alias(alias_dir, index, euid) == Recovery::Conflict {
        tracing::debug!(
          "[ipc] Alias {} at index {index} conflicts; preserving it",
          alias_dir.display()
        );
      }
    }
  }
}

/// Publish a marker then an alias for `index`; never replace an entry.
#[must_use]
pub(crate) fn publish_alias(
  alias_dir: &Path,
  index: u8,
  target: &Path,
  target_id: FileId,
  euid: u32,
) -> PublishOutcome {
  if recover_alias(alias_dir, index, euid) != Recovery::Absent {
    tracing::debug!(
      "[ipc] Skipping alias {} because the index is occupied",
      alias_dir.join(socket_name(index)).display()
    );
    return PublishOutcome::Skipped;
  }
  if let Err(err) = validate_alias_dir(alias_dir) {
    tracing::debug!(
      "[ipc] Alias dir {} rejected before publication: {err}",
      alias_dir.display()
    );
    return PublishOutcome::Skipped;
  }
  let alias = alias_dir.join(socket_name(index));
  let marker = match publish_marker(alias_dir, &alias, target, target_id, euid, index) {
    Ok(marker) => marker,
    Err(err) => {
      tracing::debug!(
        "[ipc] Marker publication failed for {}: {err}",
        alias.display()
      );
      return PublishOutcome::Skipped;
    }
  };
  if let Err(err) = std::os::unix::fs::symlink(target, &alias) {
    tracing::debug!("[ipc] Alias creation failed for {}: {err}", alias.display());
    let _ = remove_marker_if_unchanged(&marker);
    return PublishOutcome::Skipped;
  }
  match std::fs::symlink_metadata(&alias) {
    Ok(meta) if meta.file_type().is_symlink() => PublishOutcome::Created(OwnedAlias {
      index,
      alias,
      alias_id: FileId {
        dev: meta.dev(),
        ino: meta.ino(),
      },
      marker,
      target: target.to_path_buf(),
      target_id,
    }),
    Ok(_) => {
      tracing::debug!(
        "[ipc] Alias {} is not a symlink after creation; preserving it",
        alias.display()
      );
      let _ = remove_marker_if_unchanged(&marker);
      PublishOutcome::Skipped
    }
    Err(err) => {
      tracing::debug!(
        "[ipc] Alias {} vanished after creation: {err}",
        alias.display()
      );
      let _ = remove_marker_if_unchanged(&marker);
      PublishOutcome::Skipped
    }
  }
}

/// Remove an owned alias then its marker, revalidating both identities.
pub(crate) fn remove_owned_alias(owned: &OwnedAlias, euid: u32) {
  let marker = match validate_marker(&owned.marker.path, &owned.alias, euid, owned.index) {
    MarkerCheck::Valid(marker) => marker,
    MarkerCheck::Missing => {
      tracing::debug!(
        "[ipc] Marker {} is missing; preserving alias",
        owned.marker.path.display()
      );
      return;
    }
    MarkerCheck::Conflict(reason) => {
      tracing::debug!(
        "[ipc] Marker conflict for {}: {reason}",
        owned.marker.path.display()
      );
      return;
    }
  };
  let marker_target_id = FileId {
    dev: marker.data.target_dev,
    ino: marker.data.target_ino,
  };
  if marker.identity != owned.marker.identity
    || Path::new(&marker.data.alias) != owned.alias.as_path()
    || Path::new(&marker.data.target) != owned.target.as_path()
    || marker_target_id != owned.target_id
  {
    tracing::debug!(
      "[ipc] Marker {} diverges from the ownership record; preserving alias",
      marker.path.display()
    );
    return;
  }
  if stable_symlink_id(&owned.alias, &owned.target) != Some(owned.alias_id) {
    tracing::debug!(
      "[ipc] Alias {} changed since publication; preserving it",
      owned.alias.display()
    );
    return;
  }
  if let Err(err) = std::fs::remove_file(&owned.alias) {
    tracing::debug!(
      "[ipc] Alias removal failed for {}: {err}",
      owned.alias.display()
    );
    return;
  }
  if !remove_marker_if_unchanged(&marker) {
    tracing::debug!(
      "[ipc] Marker {} changed after alias removal; leaving it",
      marker.path.display()
    );
  }
}

/// Remove every owned alias, draining the vector in order.
pub(crate) fn remove_owned_aliases(owned: &mut Vec<OwnedAlias>, euid: u32) {
  for alias in owned.drain(..) {
    remove_owned_alias(&alias, euid);
  }
}

/// Apply the recovery table to a validated marker and its alias entry.
fn recover_valid_marker(alias: &Path, marker: &ValidMarker) -> Recovery {
  let target = Path::new(&marker.data.target);
  let alias_meta = match std::fs::symlink_metadata(alias) {
    Ok(meta) => meta,
    Err(err) if err.kind() == io::ErrorKind::NotFound => {
      return if target_is_preserved(&marker.data) {
        Recovery::Preserved
      } else if remove_marker_if_unchanged(marker) {
        Recovery::Absent
      } else {
        tracing::debug!(
          "[ipc] Marker {} changed before orphan removal",
          marker.path.display()
        );
        Recovery::Conflict
      };
    }
    Err(err) => {
      tracing::debug!(
        "[ipc] Alias {} could not be inspected: {err}",
        alias.display()
      );
      return Recovery::Preserved;
    }
  };
  if !alias_meta.file_type().is_symlink() {
    tracing::debug!(
      "[ipc] Alias {} is not a symlink; preserving it",
      alias.display()
    );
    return Recovery::Conflict;
  }
  match std::fs::read_link(alias) {
    Ok(link) if link == target => {}
    Ok(_) => {
      tracing::debug!(
        "[ipc] Alias {} points elsewhere; preserving it",
        alias.display()
      );
      return Recovery::Conflict;
    }
    Err(err) => {
      tracing::debug!("[ipc] Alias {} could not be read: {err}", alias.display());
      return Recovery::Conflict;
    }
  }
  if target_is_preserved(&marker.data) {
    tracing::debug!(
      "[ipc] Target {} is preserved; keeping alias",
      target.display()
    );
    return Recovery::Preserved;
  }
  if stable_symlink_id(alias, target).is_none() {
    tracing::debug!(
      "[ipc] Alias {} changed during recovery; preserving it",
      alias.display()
    );
    return Recovery::Conflict;
  }
  // This final recheck narrows, but does not close, the TOCTOU window
  // between the check and the unlink below.
  if target_is_preserved(&marker.data) {
    tracing::debug!(
      "[ipc] Target {} remains preserved before unlink; keeping alias",
      target.display()
    );
    return Recovery::Preserved;
  }
  if let Err(err) = std::fs::remove_file(alias) {
    tracing::debug!("[ipc] Alias removal failed for {}: {err}", alias.display());
    return Recovery::Conflict;
  }
  if remove_marker_if_unchanged(marker) {
    Recovery::Absent
  } else {
    tracing::debug!(
      "[ipc] Marker {} changed after alias removal",
      marker.path.display()
    );
    Recovery::Conflict
  }
}

/// Whether a marker target must be preserved: live, inconclusive, or not the
/// published object. Only a missing target or a stale socket with the recorded
/// identity authorizes orphan removal.
fn target_is_preserved(data: &MarkerData) -> bool {
  let probe = probe_socket(Path::new(&data.target));
  let recorded = FileId {
    dev: data.target_dev,
    ino: data.target_ino,
  };
  match probe.state {
    SocketState::Active | SocketState::Unknown => true,
    SocketState::Missing => false,
    SocketState::Stale => {
      if probe.identity == Some(recorded) {
        false
      } else {
        tracing::debug!(
          "[ipc] Marker target {} is stale but not the recorded object; preserving it",
          data.target
        );
        true
      }
    }
    SocketState::NotSocket => {
      tracing::debug!(
        "[ipc] Marker target {} is not a socket; preserving the foreign entry",
        data.target
      );
      true
    }
  }
}

/// Identity of `alias` when it is a symlink to `target` stable across reads.
fn stable_symlink_id(alias: &Path, target: &Path) -> Option<FileId> {
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

/// Alias/target file name for `index`.
fn socket_name(index: u8) -> String {
  format!("discord-ipc-{index}")
}

/// Validate an existing marker entry and return its opened-file identity.
fn validate_existing_marker(
  path: &Path,
  meta: &Metadata,
  expected_alias: &Path,
  euid: u32,
  index: u8,
) -> Result<ValidMarker, String> {
  let expected_name = marker_name(euid, index);
  if path.file_name().and_then(OsStr::to_str) != Some(expected_name.as_str()) {
    return Err("marker name does not match the expected name".to_owned());
  }
  check_marker_meta(meta, euid)?;

  let file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
    .open(path)
    .map_err(|err| format!("marker open failed: {err}"))?;
  let opened = file
    .metadata()
    .map_err(|err| format!("marker fstat failed: {err}"))?;
  check_marker_meta(&opened, euid)?;
  let identity = FileId {
    dev: opened.dev(),
    ino: opened.ino(),
  };

  let mut content = Vec::with_capacity(MAX_MARKER_BYTES as usize + 1);
  file
    .take(MAX_MARKER_BYTES + 1)
    .read_to_end(&mut content)
    .map_err(|err| format!("marker read failed: {err}"))?;
  if content.len() as u64 > MAX_MARKER_BYTES {
    return Err("marker is larger than 4096 bytes".to_owned());
  }

  let data: MarkerData =
    serde_json::from_slice(&content).map_err(|err| format!("marker content is invalid: {err}"))?;

  let after =
    std::fs::symlink_metadata(path).map_err(|err| format!("marker lstat failed: {err}"))?;
  if after.dev() != identity.dev || after.ino() != identity.ino {
    return Err("marker changed during validation".to_owned());
  }
  check_marker_schema(&data, expected_alias, euid, index)?;

  Ok(ValidMarker {
    path: path.to_path_buf(),
    identity,
    data,
  })
}

/// Require a regular, euid-owned, 0600 file no larger than the limit.
fn check_marker_meta(meta: &Metadata, euid: u32) -> Result<(), String> {
  if !meta.file_type().is_file() {
    return Err("marker is not a regular file".to_owned());
  }
  if meta.uid() != euid {
    return Err("marker is not owned by the effective uid".to_owned());
  }
  if meta.mode() & 0o7777 != 0o600 {
    return Err("marker mode is not 0600".to_owned());
  }
  if meta.len() > MAX_MARKER_BYTES {
    return Err("marker is larger than 4096 bytes".to_owned());
  }
  Ok(())
}

/// Require the version 1 schema and paths matching `expected_alias`.
fn check_marker_schema(
  data: &MarkerData,
  expected_alias: &Path,
  euid: u32,
  index: u8,
) -> Result<(), String> {
  if data.version != MARKER_VERSION {
    return Err(format!("unsupported marker version {}", data.version));
  }
  if data.owner_uid != euid {
    return Err("marker owner_uid does not match the effective uid".to_owned());
  }
  if expected_alias.to_str() != Some(data.alias.as_str()) {
    return Err("marker alias does not match the expected alias".to_owned());
  }
  if !Path::new(&data.alias).is_absolute() {
    return Err("marker alias is not absolute".to_owned());
  }
  let target = Path::new(&data.target);
  if !target.is_absolute() {
    return Err("marker target is not absolute".to_owned());
  }
  let expected = socket_name(index);
  if target.file_name() != Some(OsStr::new(&expected)) {
    return Err("marker target name does not match the index".to_owned());
  }
  if Path::new(&data.alias).file_name() != Some(OsStr::new(&expected)) {
    return Err("marker alias name does not match the index".to_owned());
  }
  Ok(())
}

/// Unique temporary marker path inside `alias_dir`.
fn temp_marker_path(alias_dir: &Path, euid: u32, index: u8) -> PathBuf {
  static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
  let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
  let pid = std::process::id();
  alias_dir.join(format!("{}.tmp-{pid}-{counter}", marker_name(euid, index)))
}

/// Open a temporary marker, retrying distinct candidate names on collision.
fn create_temp_marker(
  mut create: impl FnMut() -> io::Result<(File, PathBuf)>,
) -> io::Result<(File, PathBuf)> {
  for _ in 0..8 {
    match create() {
      Ok(created) => return Ok(created),
      Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
      Err(err) => return Err(err),
    }
  }
  Err(io::Error::new(
    io::ErrorKind::AlreadyExists,
    "temporary marker name exhausted",
  ))
}

/// Fill the temp file, sync it, and link it under the final name.
fn write_and_link(
  mut file: File,
  temp_path: &Path,
  final_path: &Path,
  content: &[u8],
) -> io::Result<()> {
  file.set_permissions(Permissions::from_mode(0o600))?;
  file.write_all(content)?;
  file.sync_all()?;
  drop(file);
  std::fs::hard_link(temp_path, final_path)
}

/// Remove `path` only while it still names `identity`.
fn remove_if_identity(path: &Path, identity: FileId) -> bool {
  let Ok(meta) = std::fs::symlink_metadata(path) else {
    return false;
  };
  if !meta.file_type().is_file() || meta.dev() != identity.dev || meta.ino() != identity.ino {
    return false;
  }
  std::fs::remove_file(path).is_ok()
}

/// Build an `InvalidInput` error for a rejected marker input.
fn invalid_input(message: &'static str) -> io::Error {
  io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
#[path = "../tests/unit/markers/mod.rs"]
mod tests;
