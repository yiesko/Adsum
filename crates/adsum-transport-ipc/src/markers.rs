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
  let temp_path = temp_marker_path(alias_dir, euid, index);
  let file = OpenOptions::new()
    .write(true)
    .create_new(true)
    .mode(0o600)
    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
    .open(&temp_path)?;
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
    return Recovery::Preserved;
  }
  if target_is_preserved(&marker.data) {
    tracing::debug!(
      "[ipc] Target {} became live; preserving alias",
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
  if target_is_preserved(&marker.data) {
    tracing::debug!(
      "[ipc] Target {} became live immediately before unlink; preserving alias",
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

/// Whether a marker target must be preserved (live or inconclusive).
fn target_is_preserved(data: &MarkerData) -> bool {
  let probe = probe_socket(Path::new(&data.target));
  if probe.state == SocketState::Active
    && probe.identity
      != Some(FileId {
        dev: data.target_dev,
        ino: data.target_ino,
      })
  {
    tracing::debug!(
      "[ipc] Marker target {} identity differs from its record",
      data.target
    );
  }
  match probe.state {
    SocketState::Active | SocketState::Unknown => true,
    SocketState::Missing | SocketState::Stale | SocketState::NotSocket => false,
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
mod tests {
  use std::ffi::CString;
  use std::fs::Permissions;
  use std::os::unix::ffi::OsStrExt;
  use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
  use std::os::unix::net::UnixListener;
  use std::path::{Path, PathBuf};

  use serde_json::Value;

  use super::*;
  use crate::paths::current_euid;

  /// Isolated 0700 temp dir per test (tag-suffixed, hermetic). macOS uses
  /// `/tmp` so paths stay inside the 104-byte `sun_path` limit; the base is
  /// canonicalized so platform symlinks keep the chain real.
  fn scratch(tag: &str) -> PathBuf {
    let root = if cfg!(target_os = "macos") {
      PathBuf::from("/tmp")
    } else {
      std::env::temp_dir()
    };
    let base = std::fs::canonicalize(&root).unwrap_or(root);
    let pid = std::process::id();
    let dir = base.join(format!("adsum-markers-test-{pid}-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
      .mode(0o700)
      .create(&dir)
      .expect("scratch dir");
    dir
  }

  /// `discord-ipc-<index>` path inside `dir`.
  fn socket_path(dir: &Path, index: u8) -> PathBuf {
    dir.join(format!("discord-ipc-{index}"))
  }

  /// Filesystem identity of an existing entry.
  fn file_id(path: &Path) -> FileId {
    let meta = std::fs::symlink_metadata(path).expect("entry metadata");
    FileId {
      dev: meta.dev(),
      ino: meta.ino(),
    }
  }

  /// Marker data matching `alias`/`target` for `euid`.
  fn marker_data(alias: &Path, target: &Path, euid: u32) -> MarkerData {
    let target_id = file_id(target);
    MarkerData {
      version: MARKER_VERSION,
      owner_uid: euid,
      alias: alias.to_str().expect("alias utf8").to_owned(),
      target: target.to_str().expect("target utf8").to_owned(),
      target_dev: target_id.dev,
      target_ino: target_id.ino,
    }
  }

  /// Publish a marker for `index`, creating a regular target file first.
  fn publish(alias_dir: &Path, target_dir: &Path, index: u8) -> ValidMarker {
    let alias = socket_path(alias_dir, index);
    let target = socket_path(target_dir, index);
    std::fs::write(&target, b"socket").expect("target file");
    let target_id = file_id(&target);
    publish_marker(alias_dir, &alias, &target, target_id, current_euid(), index)
      .expect("publish marker")
  }

  /// Bind a live target socket, returning it with its path and identity.
  fn bind_target(dir: &Path, index: u8) -> (UnixListener, PathBuf, FileId) {
    let path = socket_path(dir, index);
    let listener = UnixListener::bind(&path).expect("target listener");
    let identity = file_id(&path);
    (listener, path, identity)
  }

  /// Publish an alias, panicking when the index is occupied.
  fn published_owned(alias_dir: &Path, index: u8, target: &Path, target_id: FileId) -> OwnedAlias {
    match publish_alias(alias_dir, index, target, target_id, current_euid()) {
      PublishOutcome::Created(owned) => owned,
      PublishOutcome::Skipped => panic!("alias publication must succeed"),
    }
  }

  /// Write marker JSON with mode `0600`, replacing any previous content.
  fn write_marker(path: &Path, data: &MarkerData) {
    let raw = serde_json::to_vec(data).expect("serialize marker");
    std::fs::write(path, raw).expect("write marker");
    std::fs::set_permissions(path, Permissions::from_mode(0o600)).expect("chmod marker");
  }

  /// Assert `check` is a conflict without pinning the reason text.
  fn assert_conflict(label: &str, check: MarkerCheck) {
    match check {
      MarkerCheck::Conflict(_) => {}
      MarkerCheck::Valid(marker) => panic!(
        "{label}: expected conflict, got a valid marker at {}",
        marker.path.display()
      ),
      MarkerCheck::Missing => panic!("{label}: expected conflict, got missing"),
    }
  }

  /// Assert a publication error is `InvalidInput`.
  fn assert_invalid_input(label: &str, result: std::io::Result<ValidMarker>) {
    let err = result.expect_err(label);
    assert_eq!(
      err.kind(),
      std::io::ErrorKind::InvalidInput,
      "{label}: {err}"
    );
  }

  /// Marker names are qualified by the effective uid and index.
  #[test]
  fn marker_name_includes_uid_and_index() {
    assert_eq!(marker_name(1000, 0), ".adsum-ipc-owner-1000-0");
    assert_eq!(
      marker_path(Path::new("/tmp"), 1000, 9),
      PathBuf::from("/tmp/.adsum-ipc-owner-1000-9")
    );
  }

  /// Publication writes a 0600 regular file whose JSON matches the inputs.
  #[test]
  fn publish_marker_writes_0600_json() {
    let alias_dir = scratch("publish");
    let target_dir = scratch("publish-target");
    let euid = current_euid();
    let index = 0;
    let alias = socket_path(&alias_dir, index);
    let target = socket_path(&target_dir, index);
    std::fs::write(&target, b"socket").expect("target file");
    let target_id = file_id(&target);

    let marker =
      publish_marker(&alias_dir, &alias, &target, target_id, euid, index).expect("publish marker");

    let expected_path = marker_path(&alias_dir, euid, index);
    assert_eq!(marker.path, expected_path);
    let meta = std::fs::symlink_metadata(&expected_path).expect("marker metadata");
    assert!(meta.file_type().is_file(), "marker must be a regular file");
    assert_eq!(meta.mode() & 0o777, 0o600);
    assert_eq!(meta.uid(), euid);
    assert!(meta.len() <= MAX_MARKER_BYTES);
    assert_eq!(marker.identity, file_id(&expected_path));
    assert_eq!(marker.data, marker_data(&alias, &target, euid));

    let raw = std::fs::read(&expected_path).expect("marker content");
    let json: Value = serde_json::from_slice(&raw).expect("marker json");
    assert_eq!(json["version"].as_u64(), Some(u64::from(MARKER_VERSION)));
    assert_eq!(json["owner_uid"].as_u64(), Some(u64::from(euid)));
    assert_eq!(json["alias"].as_str(), alias.to_str());
    assert_eq!(json["target"].as_str(), target.to_str());
    assert_eq!(json["target_dev"].as_u64(), Some(target_id.dev));
    assert_eq!(json["target_ino"].as_u64(), Some(target_id.ino));
    assert_eq!(
      std::fs::read_dir(&alias_dir)
        .expect("read alias dir")
        .count(),
      1,
      "publication must not leave a temporary file"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A second publication fails without replacing the existing marker.
  #[test]
  fn publish_marker_never_replaces_existing_marker() {
    let alias_dir = scratch("no-replace");
    let target_dir = scratch("no-replace-target");
    let other_target_dir = scratch("no-replace-other");
    let euid = current_euid();
    let index = 0;
    let marker = publish(&alias_dir, &target_dir, index);
    let original = std::fs::read(&marker.path).expect("original content");
    let original_id = file_id(&marker.path);

    let other_target = socket_path(&other_target_dir, index);
    std::fs::write(&other_target, b"other").expect("other target");
    let second = publish_marker(
      &alias_dir,
      &socket_path(&alias_dir, index),
      &other_target,
      file_id(&other_target),
      euid,
      index,
    )
    .expect_err("second publication must fail");

    assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
      std::fs::read(&marker.path).expect("original kept"),
      original
    );
    assert_eq!(file_id(&marker.path), original_id);
    assert_eq!(
      std::fs::read_dir(&alias_dir)
        .expect("read alias dir")
        .count(),
      1,
      "failed publication must remove its temporary file"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
    let _ = std::fs::remove_dir_all(&other_target_dir);
  }

  /// A symlink is never followed, even when its target content would validate.
  #[test]
  fn validate_marker_rejects_symlink() {
    let source_dir = scratch("symlink-source");
    let alias_dir = scratch("symlink-marker");
    let target_dir = scratch("symlink-target");
    let euid = current_euid();
    let index = 0;
    let alias = socket_path(&alias_dir, index);
    let target = socket_path(&target_dir, index);
    std::fs::write(&target, b"socket").expect("target file");

    let source = marker_path(&source_dir, euid, index);
    write_marker(&source, &marker_data(&alias, &target, euid));
    let marker = marker_path(&alias_dir, euid, index);
    std::os::unix::fs::symlink(&source, &marker).expect("marker symlink");

    assert_conflict("symlink", validate_marker(&marker, &alias, euid, index));
    assert!(
      std::fs::symlink_metadata(&marker)
        .expect("marker kept")
        .file_type()
        .is_symlink(),
      "validation must not unlink or follow the marker"
    );

    let _ = std::fs::remove_dir_all(&source_dir);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// Divergent alias, target, index, and owner fields are conflicts.
  #[test]
  fn validate_marker_rejects_divergent_alias_or_target() {
    let alias_dir = scratch("divergent");
    let target_dir = scratch("divergent-target");
    let other_dir = scratch("divergent-other");
    let euid = current_euid();
    let index = 0;
    let marker = publish(&alias_dir, &target_dir, index);
    let alias = socket_path(&alias_dir, index);
    let target = socket_path(&target_dir, index);
    let marker_file = marker.path.clone();

    let mut wrong_alias = marker_data(&alias, &target, euid);
    wrong_alias.alias = socket_path(&other_dir, index)
      .to_str()
      .expect("other alias utf8")
      .to_owned();
    write_marker(&marker_file, &wrong_alias);
    assert_conflict(
      "wrong alias",
      validate_marker(&marker_file, &alias, euid, index),
    );

    let relative_alias = Path::new("discord-ipc-0");
    let mut relative_alias_data = marker_data(&alias, &target, euid);
    relative_alias_data.alias = "discord-ipc-0".to_owned();
    write_marker(&marker_file, &relative_alias_data);
    assert_conflict(
      "relative alias",
      validate_marker(&marker_file, relative_alias, euid, index),
    );

    let mut relative_target = marker_data(&alias, &target, euid);
    relative_target.target = "discord-ipc-0".to_owned();
    write_marker(&marker_file, &relative_target);
    assert_conflict(
      "relative target",
      validate_marker(&marker_file, &alias, euid, index),
    );

    let mut wrong_target_index = marker_data(&alias, &target, euid);
    wrong_target_index.target = socket_path(&target_dir, 1)
      .to_str()
      .expect("wrong target utf8")
      .to_owned();
    write_marker(&marker_file, &wrong_target_index);
    assert_conflict(
      "wrong target index",
      validate_marker(&marker_file, &alias, euid, index),
    );

    let mut wrong_owner = marker_data(&alias, &target, euid);
    wrong_owner.owner_uid = euid.wrapping_add(1);
    write_marker(&marker_file, &wrong_owner);
    assert_conflict(
      "wrong owner",
      validate_marker(&marker_file, &alias, euid, index),
    );

    let wrong_index_alias = socket_path(&alias_dir, 1);
    let mut wrong_alias_index = marker_data(&wrong_index_alias, &target, euid);
    wrong_alias_index.alias = wrong_index_alias
      .to_str()
      .expect("wrong index alias utf8")
      .to_owned();
    write_marker(&marker_file, &wrong_alias_index);
    assert_conflict(
      "wrong alias index",
      validate_marker(&marker_file, &wrong_index_alias, euid, index),
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
    let _ = std::fs::remove_dir_all(&other_dir);
  }

  /// The marker basename and schema version are part of validation.
  #[test]
  fn validate_marker_rejects_wrong_basename_and_version() {
    let alias_dir = scratch("basename");
    let target_dir = scratch("basename-target");
    let euid = current_euid();
    let index = 0;
    let marker = publish(&alias_dir, &target_dir, index);
    let alias = socket_path(&alias_dir, index);
    let target = socket_path(&target_dir, index);

    let wrong_name = alias_dir.join("not-a-marker");
    std::fs::rename(&marker.path, &wrong_name).expect("rename marker");
    assert_conflict(
      "wrong basename",
      validate_marker(&wrong_name, &alias, euid, index),
    );

    std::fs::rename(&wrong_name, &marker.path).expect("restore marker name");
    let mut wrong_version = marker_data(&alias, &target, euid);
    wrong_version.version = MARKER_VERSION + 1;
    write_marker(&marker.path, &wrong_version);
    assert_conflict(
      "wrong version",
      validate_marker(&marker.path, &alias, euid, index),
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A 4097-byte marker is over the limit and preserved.
  #[test]
  fn validate_marker_rejects_oversized_file() {
    let alias_dir = scratch("oversized");
    let euid = current_euid();
    let index = 0;
    let alias = socket_path(&alias_dir, index);
    let marker = marker_path(&alias_dir, euid, index);
    let oversized = vec![b'x'; MAX_MARKER_BYTES as usize + 1];
    std::fs::write(&marker, &oversized).expect("oversized marker");
    std::fs::set_permissions(&marker, Permissions::from_mode(0o600)).expect("chmod marker");

    assert_conflict("oversized", validate_marker(&marker, &alias, euid, index));
    assert_eq!(
      std::fs::symlink_metadata(&marker)
        .expect("marker kept")
        .len(),
      MAX_MARKER_BYTES + 1
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
  }

  /// A mode other than 0600 is a conflict; restoring it validates again.
  #[test]
  fn validate_marker_rejects_non_0600_mode() {
    let alias_dir = scratch("mode");
    let target_dir = scratch("mode-target");
    let euid = current_euid();
    let index = 0;
    let marker = publish(&alias_dir, &target_dir, index);
    let alias = socket_path(&alias_dir, index);

    std::fs::set_permissions(&marker.path, Permissions::from_mode(0o644)).expect("chmod 0644");
    assert_conflict("0644", validate_marker(&marker.path, &alias, euid, index));
    assert!(
      std::fs::symlink_metadata(&marker.path).is_ok(),
      "validation must not unlink the marker"
    );

    std::fs::set_permissions(&marker.path, Permissions::from_mode(0o600)).expect("restore 0600");
    match validate_marker(&marker.path, &alias, euid, index) {
      MarkerCheck::Valid(valid) => assert_eq!(valid.identity, marker.identity),
      other => panic!("restored marker must validate: {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A marker owned by another uid is a conflict and stays on disk.
  #[test]
  fn validate_marker_rejects_foreign_owned_file() {
    if current_euid() != 0 {
      return;
    }
    let alias_dir = scratch("foreign-owner");
    let target_dir = scratch("foreign-owner-target");
    let euid = current_euid();
    let index = 0;
    let marker = publish(&alias_dir, &target_dir, index);
    let alias = socket_path(&alias_dir, index);
    let path = CString::new(marker.path.as_os_str().as_bytes()).expect("path without nul");

    // SAFETY: `path` is a NUL-terminated string naming the marker created
    // above; chown only changes its owner and never outlives the buffer.
    assert_eq!(unsafe { libc::chown(path.as_ptr(), 65534, 65534) }, 0);

    assert_conflict(
      "foreign owner",
      validate_marker(&marker.path, &alias, euid, index),
    );
    assert!(
      std::fs::symlink_metadata(&marker.path).is_ok(),
      "foreign-owned marker must not be deleted"
    );

    // SAFETY: same valid path; restoring ownership lets the test clean up.
    assert_eq!(unsafe { libc::chown(path.as_ptr(), 0, 0) }, 0);

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A file owned by a different euid than the one being validated conflicts.
  #[test]
  fn validate_marker_rejects_mismatched_file_owner() {
    let alias_dir = scratch("owner-mismatch");
    let target_dir = scratch("owner-mismatch-target");
    let euid = current_euid();
    let other_euid = euid.wrapping_add(1);
    let index = 0;
    let alias = socket_path(&alias_dir, index);
    let target = socket_path(&target_dir, index);
    std::fs::write(&target, b"socket").expect("target file");
    let target_id = file_id(&target);
    let data = MarkerData {
      version: MARKER_VERSION,
      owner_uid: other_euid,
      alias: alias.to_str().expect("alias utf8").to_owned(),
      target: target.to_str().expect("target utf8").to_owned(),
      target_dev: target_id.dev,
      target_ino: target_id.ino,
    };
    let marker = marker_path(&alias_dir, other_euid, index);
    write_marker(&marker, &data);

    assert_conflict(
      "file owner",
      validate_marker(&marker, &alias, other_euid, index),
    );
    assert!(
      std::fs::symlink_metadata(&marker).is_ok(),
      "mismatched owner must not be deleted"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A matching marker is removed by identity.
  #[test]
  fn remove_marker_if_unchanged_removes_matching_marker() {
    let alias_dir = scratch("unlink-match");
    let target_dir = scratch("unlink-match-target");
    let marker = publish(&alias_dir, &target_dir, 0);

    assert!(remove_marker_if_unchanged(&marker));
    assert!(
      std::fs::symlink_metadata(&marker.path).is_err(),
      "matching marker must be removed"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A replaced marker is preserved because its identity no longer matches.
  #[test]
  fn remove_marker_if_unchanged_preserves_replaced_marker() {
    let alias_dir = scratch("unlink-replaced");
    let target_dir = scratch("unlink-replaced-target");
    let marker = publish(&alias_dir, &target_dir, 0);

    // Create the replacement before the rename so its inode cannot reuse the
    // removed marker's number; rename then replaces the entry atomically.
    let replacement = alias_dir.join("replacement");
    std::fs::write(&replacement, b"replacement").expect("replacement file");
    std::fs::set_permissions(&replacement, Permissions::from_mode(0o600))
      .expect("chmod replacement");
    std::fs::rename(&replacement, &marker.path).expect("replace marker");
    assert_ne!(
      file_id(&marker.path),
      marker.identity,
      "replacement must have a distinct identity"
    );

    assert!(!remove_marker_if_unchanged(&marker));
    assert_eq!(
      std::fs::read(&marker.path).expect("replacement kept"),
      b"replacement"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A published marker round-trips through validation.
  #[test]
  fn validate_marker_accepts_published_marker() {
    let alias_dir = scratch("accept");
    let target_dir = scratch("accept-target");
    let euid = current_euid();
    let index = 0;
    let marker = publish(&alias_dir, &target_dir, index);
    let alias = socket_path(&alias_dir, index);

    match validate_marker(&marker.path, &alias, euid, index) {
      MarkerCheck::Valid(valid) => {
        assert_eq!(valid.path, marker.path);
        assert_eq!(valid.identity, marker.identity);
        assert_eq!(valid.data, marker.data);
      }
      other => panic!("published marker must validate: {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// An absent marker reports `Missing`, not a conflict.
  #[test]
  fn validate_marker_reports_missing() {
    let alias_dir = scratch("missing");
    let euid = current_euid();
    let index = 0;
    let marker = marker_path(&alias_dir, euid, index);
    let alias = socket_path(&alias_dir, index);

    match validate_marker(&marker, &alias, euid, index) {
      MarkerCheck::Missing => {}
      other => panic!("expected missing, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&alias_dir);
  }

  /// Publication refuses inputs that validation would reject.
  #[test]
  fn publish_marker_rejects_invalid_paths() {
    let alias_dir = scratch("invalid-input");
    let target_dir = scratch("invalid-input-target");
    let other_dir = scratch("invalid-input-other");
    let euid = current_euid();
    let index = 0;
    let alias = socket_path(&alias_dir, index);
    let target = socket_path(&target_dir, index);
    std::fs::write(&target, b"socket").expect("target file");
    let target_id = file_id(&target);

    assert_invalid_input(
      "relative target",
      publish_marker(
        &alias_dir,
        &alias,
        Path::new("discord-ipc-0"),
        target_id,
        euid,
        index,
      ),
    );
    let wrong_index = socket_path(&alias_dir, 1);
    assert_invalid_input(
      "wrong index",
      publish_marker(&alias_dir, &wrong_index, &target, target_id, euid, index),
    );
    assert_invalid_input(
      "wrong parent",
      publish_marker(&other_dir, &alias, &target, target_id, euid, index),
    );
    let non_utf8 = alias_dir.join(std::ffi::OsStr::from_bytes(b"discord-ipc-\xff"));
    assert_invalid_input(
      "non-utf8 alias",
      publish_marker(&alias_dir, &non_utf8, &target, target_id, euid, index),
    );
    let non_utf8_target = target_dir.join(std::ffi::OsStr::from_bytes(b"discord-ipc-\xff"));
    assert_invalid_input(
      "non-utf8 target",
      publish_marker(&alias_dir, &alias, &non_utf8_target, target_id, euid, index),
    );
    assert_invalid_input(
      "relative alias",
      publish_marker(
        Path::new(""),
        Path::new("discord-ipc-0"),
        &target,
        target_id,
        euid,
        index,
      ),
    );
    let wrong_target_index = socket_path(&target_dir, 1);
    std::fs::write(&wrong_target_index, b"other").expect("wrong target");
    assert_invalid_input(
      "target index",
      publish_marker(
        &alias_dir,
        &alias,
        &wrong_target_index,
        file_id(&wrong_target_index),
        euid,
        index,
      ),
    );
    assert_eq!(
      std::fs::read_dir(&alias_dir)
        .expect("read alias dir")
        .count(),
      0,
      "rejected inputs must not create a temporary file"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
    let _ = std::fs::remove_dir_all(&other_dir);
  }

  /// Content above the marker limit is refused before any file is created.
  #[test]
  fn publish_marker_rejects_content_over_limit() {
    let long_dir = PathBuf::from(format!("/tmp/{}", "x".repeat(MAX_MARKER_BYTES as usize)));
    let alias = socket_path(&long_dir, 0);
    let target = socket_path(Path::new("/tmp"), 0);
    let result = publish_marker(
      &long_dir,
      &alias,
      &target,
      FileId { dev: 0, ino: 0 },
      current_euid(),
      0,
    );
    assert_invalid_input("oversized content", result);
  }

  /// Publication creates a symlink and a marker recording both identities.
  #[test]
  fn publish_alias_creates_symlink_then_marker() {
    let alias_dir = scratch("alias-create");
    let target_dir = scratch("alias-create-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);

    let PublishOutcome::Created(owned) = publish_alias(&alias_dir, index, &target, target_id, euid)
    else {
      panic!("publish_alias must create the alias");
    };
    let alias = socket_path(&alias_dir, index);
    assert_eq!(std::fs::read_link(&alias).expect("alias link"), target);
    assert_eq!(owned.index, index);
    assert_eq!(owned.alias, alias);
    assert_eq!(owned.alias_id, file_id(&alias));
    assert_eq!(owned.target, target);
    assert_eq!(owned.target_id, target_id);
    assert_eq!(owned.marker.path, marker_path(&alias_dir, euid, index));
    assert_eq!(owned.marker.data, marker_data(&alias, &target, euid));
    assert_eq!(owned.marker.data.target_dev, target_id.dev);
    assert_eq!(owned.marker.data.target_ino, target_id.ino);

    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A markerless foreign symlink is never replaced or adopted.
  #[test]
  fn publish_alias_preserves_foreign_symlink_without_marker() {
    let alias_dir = scratch("alias-foreign-link");
    let target_dir = scratch("alias-foreign-link-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let foreign_target = target_dir.join("foreign");
    std::fs::write(&foreign_target, b"foreign").expect("foreign target");
    let alias = socket_path(&alias_dir, index);
    std::os::unix::fs::symlink(&foreign_target, &alias).expect("foreign alias");

    assert!(matches!(
      publish_alias(&alias_dir, index, &target, target_id, euid),
      PublishOutcome::Skipped
    ));
    assert_eq!(
      std::fs::read_link(&alias).expect("foreign link kept"),
      foreign_target
    );
    assert!(
      std::fs::symlink_metadata(marker_path(&alias_dir, euid, index)).is_err(),
      "a preserved foreign link must not gain a marker"
    );

    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// Regular files, sockets, directories, and symlinks all block publication.
  #[test]
  fn publish_alias_preserves_foreign_entries() {
    let alias_dir = scratch("alias-foreign");
    let target_dir = scratch("alias-foreign-target");
    let euid = current_euid();

    let (listener0, target0, target_id0) = bind_target(&target_dir, 0);
    let foreign_file = socket_path(&alias_dir, 0);
    std::fs::write(&foreign_file, b"keep").expect("foreign file");

    let (listener1, target1, target_id1) = bind_target(&target_dir, 1);
    let foreign_socket = socket_path(&alias_dir, 1);
    let foreign_listener = UnixListener::bind(&foreign_socket).expect("foreign socket");

    let (listener2, target2, target_id2) = bind_target(&target_dir, 2);
    let foreign_dir = socket_path(&alias_dir, 2);
    std::fs::create_dir(&foreign_dir).expect("foreign dir");

    let (listener3, target3, target_id3) = bind_target(&target_dir, 3);
    let foreign_link = socket_path(&alias_dir, 3);
    let link_target = target_dir.join("link-target");
    std::fs::write(&link_target, b"link").expect("link target");
    std::os::unix::fs::symlink(&link_target, &foreign_link).expect("foreign link");

    for (index, target, target_id) in [
      (0, &target0, target_id0),
      (1, &target1, target_id1),
      (2, &target2, target_id2),
      (3, &target3, target_id3),
    ] {
      assert!(
        matches!(
          publish_alias(&alias_dir, index, target, target_id, euid),
          PublishOutcome::Skipped
        ),
        "index {index} must be skipped"
      );
      assert!(
        std::fs::symlink_metadata(marker_path(&alias_dir, euid, index)).is_err(),
        "index {index} must not gain a marker"
      );
    }

    assert_eq!(
      std::fs::read(&foreign_file).expect("foreign file kept"),
      b"keep"
    );
    assert!(
      std::fs::symlink_metadata(&foreign_socket)
        .expect("foreign socket kept")
        .file_type()
        .is_socket()
    );
    assert!(
      std::fs::symlink_metadata(&foreign_dir)
        .expect("foreign dir kept")
        .file_type()
        .is_dir()
    );
    assert_eq!(
      std::fs::read_link(&foreign_link).expect("foreign link kept"),
      link_target
    );

    drop(foreign_listener);
    drop(listener0);
    drop(listener1);
    drop(listener2);
    drop(listener3);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A foreign-owned marker blocks publication and is never deleted.
  #[test]
  fn publish_alias_preserves_foreign_owned_marker() {
    if current_euid() != 0 {
      return;
    }
    let alias_dir = scratch("alias-foreign-marker");
    let target_dir = scratch("alias-foreign-marker-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let alias = socket_path(&alias_dir, index);
    let marker = marker_path(&alias_dir, euid, index);
    write_marker(&marker, &marker_data(&alias, &target, euid));
    let path = CString::new(marker.as_os_str().as_bytes()).expect("path without nul");
    // SAFETY: `path` is a NUL-terminated string naming the marker created
    // above; chown only changes its owner and never outlives the buffer.
    assert_eq!(unsafe { libc::chown(path.as_ptr(), 65534, 65534) }, 0);

    assert!(matches!(
      publish_alias(&alias_dir, index, &target, target_id, euid),
      PublishOutcome::Skipped
    ));
    assert_eq!(
      std::fs::symlink_metadata(&marker)
        .expect("foreign marker kept")
        .uid(),
      65534
    );
    assert!(
      std::fs::symlink_metadata(&alias).is_err(),
      "a foreign marker must not authorize an alias"
    );

    // SAFETY: same valid path; restoring ownership lets the test clean up.
    assert_eq!(unsafe { libc::chown(path.as_ptr(), 0, 0) }, 0);
    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// Removing an owned alias unlinks the alias and then its marker.
  #[test]
  fn remove_owned_alias_removes_alias_then_marker() {
    let alias_dir = scratch("owned-remove");
    let target_dir = scratch("owned-remove-target");
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let owned = published_owned(&alias_dir, index, &target, target_id);
    let alias = socket_path(&alias_dir, index);
    let marker = owned.marker.path.clone();

    remove_owned_alias(&owned, current_euid());
    assert!(std::fs::symlink_metadata(&alias).is_err());
    assert!(std::fs::symlink_metadata(&marker).is_err());

    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A missing, changed, or divergent marker preserves the alias.
  #[test]
  fn remove_owned_alias_preserves_when_marker_missing_invalid_or_divergent() {
    let euid = current_euid();

    let alias_dir = scratch("owned-missing");
    let target_dir = scratch("owned-missing-target");
    let (listener_a, target_a, id_a) = bind_target(&target_dir, 0);
    let owned = published_owned(&alias_dir, 0, &target_a, id_a);
    std::fs::remove_file(&owned.marker.path).expect("delete marker");
    remove_owned_alias(&owned, euid);
    assert!(
      std::fs::symlink_metadata(&owned.alias).is_ok(),
      "alias must survive a missing marker"
    );
    drop(listener_a);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);

    let alias_dir = scratch("owned-divergent-marker");
    let target_dir = scratch("owned-divergent-marker-target");
    let (listener_b, target_b, id_b) = bind_target(&target_dir, 0);
    let owned = published_owned(&alias_dir, 0, &target_b, id_b);
    let mut divergent = marker_data(&owned.alias, &owned.target, euid);
    divergent.target_ino = divergent.target_ino.wrapping_add(1);
    write_marker(&owned.marker.path, &divergent);
    remove_owned_alias(&owned, euid);
    assert!(
      std::fs::symlink_metadata(&owned.alias).is_ok(),
      "alias must survive a divergent marker"
    );
    assert!(
      std::fs::symlink_metadata(&owned.marker.path).is_ok(),
      "divergent marker must stay"
    );
    drop(listener_b);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);

    let alias_dir = scratch("owned-divergent-alias");
    let target_dir = scratch("owned-divergent-alias-target");
    let (listener_c, target_c, id_c) = bind_target(&target_dir, 0);
    let owned = published_owned(&alias_dir, 0, &target_c, id_c);
    let elsewhere = target_dir.join("elsewhere");
    std::fs::write(&elsewhere, b"elsewhere").expect("elsewhere");
    std::fs::remove_file(&owned.alias).expect("remove alias");
    std::os::unix::fs::symlink(&elsewhere, &owned.alias).expect("divergent alias");
    remove_owned_alias(&owned, euid);
    assert_eq!(
      std::fs::read_link(&owned.alias).expect("alias kept"),
      elsewhere
    );
    assert!(
      std::fs::symlink_metadata(&owned.marker.path).is_ok(),
      "marker must stay with a divergent alias"
    );
    drop(listener_c);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A replaced alias symlink is preserved because its identity changed.
  #[test]
  fn remove_owned_alias_preserves_when_alias_identity_changed() {
    let alias_dir = scratch("owned-alias-id");
    let target_dir = scratch("owned-alias-id-target");
    let (listener, target, target_id) = bind_target(&target_dir, 0);
    let owned = published_owned(&alias_dir, 0, &target, target_id);
    let replacement = alias_dir.join("replacement");
    std::os::unix::fs::symlink(&owned.target, &replacement).expect("replacement link");
    std::fs::rename(&replacement, &owned.alias).expect("replace alias");
    assert_ne!(file_id(&owned.alias), owned.alias_id);

    remove_owned_alias(&owned, current_euid());
    assert_eq!(
      std::fs::read_link(&owned.alias).expect("alias kept"),
      owned.target
    );
    assert!(
      std::fs::symlink_metadata(&owned.marker.path).is_ok(),
      "marker must stay when the alias identity changed"
    );

    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// An orphan marker with a dead target is removed after validation.
  #[test]
  fn orphan_marker_with_dead_target_is_removed() {
    let alias_dir = scratch("orphan-dead");
    let target_dir = scratch("orphan-dead-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let alias = socket_path(&alias_dir, index);
    let marker = publish_marker(&alias_dir, &alias, &target, target_id, euid, index)
      .expect("publish orphan marker");
    drop(listener);

    assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Absent);
    assert!(std::fs::symlink_metadata(&marker.path).is_err());
    assert!(std::fs::symlink_metadata(&alias).is_err());

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// An orphan marker whose target still accepts connections is preserved.
  #[test]
  fn orphan_marker_with_live_target_is_preserved() {
    let alias_dir = scratch("orphan-live");
    let target_dir = scratch("orphan-live-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let alias = socket_path(&alias_dir, index);
    let marker = publish_marker(&alias_dir, &alias, &target, target_id, euid, index)
      .expect("publish orphan marker");

    assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Preserved);
    assert!(std::fs::symlink_metadata(&marker.path).is_ok());
    assert!(std::fs::symlink_metadata(&alias).is_err());

    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// A valid alias whose target is dead is removed with its marker.
  #[test]
  fn orphan_alias_with_valid_marker_and_dead_target_is_removed() {
    let alias_dir = scratch("orphan-alias-dead");
    let target_dir = scratch("orphan-alias-dead-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let owned = published_owned(&alias_dir, index, &target, target_id);
    drop(listener);

    assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Absent);
    assert!(std::fs::symlink_metadata(&owned.alias).is_err());
    assert!(std::fs::symlink_metadata(&owned.marker.path).is_err());

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// An alias pointing elsewhere is a conflict; both entries are preserved.
  #[test]
  fn orphan_alias_with_divergent_target_is_preserved() {
    let alias_dir = scratch("orphan-alias-divergent");
    let target_dir = scratch("orphan-alias-divergent-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let owned = published_owned(&alias_dir, index, &target, target_id);
    let elsewhere = target_dir.join("elsewhere");
    std::fs::write(&elsewhere, b"elsewhere").expect("elsewhere");
    std::fs::remove_file(&owned.alias).expect("remove alias");
    std::os::unix::fs::symlink(&elsewhere, &owned.alias).expect("divergent alias");

    assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Conflict);
    assert_eq!(
      std::fs::read_link(&owned.alias).expect("alias kept"),
      elsewhere
    );
    assert!(
      std::fs::symlink_metadata(&owned.marker.path).is_ok(),
      "marker must stay with a divergent alias"
    );

    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// An abrupt exit leaves entries that the next recovery removes.
  #[test]
  fn abrupt_exit_state_is_recovered() {
    let alias_dir = scratch("abrupt-exit");
    let target_dir = scratch("abrupt-exit-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let owned = published_owned(&alias_dir, index, &target, target_id);
    drop(listener);

    assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Absent);
    assert!(std::fs::symlink_metadata(&owned.alias).is_err());
    assert!(std::fs::symlink_metadata(&owned.marker.path).is_err());
    assert!(
      std::fs::symlink_metadata(&target).is_ok(),
      "recovery removes the alias and marker, not the stale target"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// Concurrent publications produce exactly one owner and one marker.
  #[test]
  fn concurrent_marker_publication_keeps_one_owner() {
    let alias_dir = scratch("alias-race");
    let target_dir = scratch("alias-race-target");
    let euid = current_euid();
    let index = 0;
    let (listener, target, target_id) = bind_target(&target_dir, index);
    let barrier = std::sync::Barrier::new(2);

    let created = std::thread::scope(|scope| {
      let first = scope.spawn(|| {
        barrier.wait();
        matches!(
          publish_alias(&alias_dir, index, &target, target_id, euid),
          PublishOutcome::Created(_)
        )
      });
      let second = scope.spawn(|| {
        barrier.wait();
        matches!(
          publish_alias(&alias_dir, index, &target, target_id, euid),
          PublishOutcome::Created(_)
        )
      });
      [
        first.join().expect("first thread"),
        second.join().expect("second thread"),
      ]
    });
    assert_eq!(
      created.iter().filter(|created| **created).count(),
      1,
      "exactly one publication must win"
    );
    let alias = socket_path(&alias_dir, index);
    assert_eq!(std::fs::read_link(&alias).expect("winner alias"), target);
    assert!(
      std::fs::symlink_metadata(marker_path(&alias_dir, euid, index)).is_ok(),
      "the winner's marker must survive"
    );

    drop(listener);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// Scanning alias dirs removes orphan markers and keeps foreign entries.
  #[test]
  fn recover_orphan_aliases_removes_only_orphan_markers() {
    let alias_dir = scratch("orphan-scan");
    let target_dir = scratch("orphan-scan-target");
    let euid = current_euid();
    let (listener, target, target_id) = bind_target(&target_dir, 0);
    let alias = socket_path(&alias_dir, 0);
    let marker = publish_marker(&alias_dir, &alias, &target, target_id, euid, 0)
      .expect("publish orphan marker");
    drop(listener);
    let foreign = socket_path(&alias_dir, 1);
    std::fs::write(&foreign, b"foreign").expect("foreign entry");

    recover_orphan_aliases(std::slice::from_ref(&alias_dir), euid);

    assert!(
      std::fs::symlink_metadata(&marker.path).is_err(),
      "orphan marker must be removed"
    );
    assert_eq!(
      std::fs::read(&foreign).expect("foreign entry kept"),
      b"foreign"
    );

    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }

  /// Draining owned aliases removes every alias and empties the vector.
  #[test]
  fn remove_owned_aliases_drains_in_order() {
    let alias_dir = scratch("owned-drain");
    let target_dir = scratch("owned-drain-target");
    let euid = current_euid();
    let (listener0, target0, id0) = bind_target(&target_dir, 0);
    let (listener1, target1, id1) = bind_target(&target_dir, 1);
    let mut owned = vec![
      published_owned(&alias_dir, 0, &target0, id0),
      published_owned(&alias_dir, 1, &target1, id1),
    ];

    remove_owned_aliases(&mut owned, euid);

    assert!(owned.is_empty());
    for index in 0..=1 {
      assert!(
        std::fs::symlink_metadata(socket_path(&alias_dir, index)).is_err(),
        "alias {index} must be removed"
      );
      assert!(
        std::fs::symlink_metadata(marker_path(&alias_dir, euid, index)).is_err(),
        "marker {index} must be removed"
      );
    }

    drop(listener0);
    drop(listener1);
    let _ = std::fs::remove_dir_all(&alias_dir);
    let _ = std::fs::remove_dir_all(&target_dir);
  }
}
