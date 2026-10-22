//! Alias ownership markers: schema, validation, and atomic publication.
//!
//! A marker is never overwritten and never authorizes removing an alias
//! unless its recorded `(dev, ino)` still matches the on-disk file.
#![allow(dead_code)] // Staging for Task 4, which consumes these APIs.

use std::ffi::OsStr;
use std::fs::{File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::probe::FileId;

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
  use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
  use std::path::{Path, PathBuf};

  use serde_json::Value;

  use super::*;
  use crate::paths::current_euid;

  /// Isolated 0700 temp dir per test (tag-suffixed, hermetic). The temp base
  /// is canonicalized so platform symlinks (macOS `/var`) keep the chain real.
  fn scratch(tag: &str) -> PathBuf {
    let base = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
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
}
