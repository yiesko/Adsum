use std::ffi::CString;
use std::fs::Permissions;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::*;
use crate::paths::current_euid;

mod aliases;
mod cleanup;
mod publication;
mod recovery;
mod validation;

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

/// Write raw marker bytes with mode `0600`, replacing any previous content.
fn write_raw_marker(path: &Path, bytes: &[u8]) {
  std::fs::write(path, bytes).expect("write raw marker");
  std::fs::set_permissions(path, Permissions::from_mode(0o600)).expect("chmod raw marker");
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
