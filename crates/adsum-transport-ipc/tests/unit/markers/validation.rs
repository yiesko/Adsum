use super::*;

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

/// Empty, truncated, incomplete, or mistyped markers are conflicts and stay.
#[test]
fn validate_marker_rejects_malformed_content() {
  let alias_dir = scratch("malformed");
  let euid = current_euid();
  let index = 5;
  let alias = socket_path(&alias_dir, index);
  let marker = marker_path(&alias_dir, euid, index);
  let cases: [&[u8]; 5] = [
    b"",
    br#"{"version":1"#,
    b"{}",
    br#"{"version":1}"#,
    br#"{"version":"1"}"#,
  ];

  for bytes in cases {
    write_raw_marker(&marker, bytes);
    assert_conflict(
      std::str::from_utf8(bytes).expect("utf8 case"),
      validate_marker(&marker, &alias, euid, index),
    );
    assert!(
      std::fs::symlink_metadata(&marker).is_ok(),
      "invalid marker must not be removed"
    );
  }

  let _ = std::fs::remove_dir_all(&alias_dir);
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
