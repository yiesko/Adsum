use super::*;

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

/// Temporary marker creation retries distinct names after collisions.
#[test]
fn create_temp_marker_retries_collisions() {
  let dir = scratch("temp-marker-retry");
  let mut attempts = 0;
  let (mut file, path) = create_temp_marker(|| {
    attempts += 1;
    if attempts <= 2 {
      return Err(io::Error::new(io::ErrorKind::AlreadyExists, "taken"));
    }
    let path = dir.join(format!("marker-{attempts}"));
    let file = OpenOptions::new()
      .write(true)
      .create_new(true)
      .mode(0o600)
      .open(&path)?;
    Ok((file, path))
  })
  .expect("third attempt must succeed");

  assert_eq!(attempts, 3, "two collisions must be retried");
  assert!(path.is_file(), "created marker must stay on disk");
  file.write_all(b"payload").expect("write marker");

  let _ = std::fs::remove_dir_all(&dir);
}

/// Errors other than a name collision propagate without retrying.
#[test]
fn create_temp_marker_propagates_other_errors() {
  let mut attempts = 0;
  let err = create_temp_marker(|| {
    attempts += 1;
    Err(io::Error::from(io::ErrorKind::PermissionDenied))
  })
  .expect_err("non-collision error must propagate");

  assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
  assert_eq!(attempts, 1, "other errors must not be retried");
}

/// Eight collisions exhaust the temporary-name budget.
#[test]
fn create_temp_marker_exhausts_after_eight_collisions() {
  let mut attempts = 0;
  let err = create_temp_marker(|| {
    attempts += 1;
    Err::<(File, PathBuf), _>(io::Error::new(io::ErrorKind::AlreadyExists, "taken"))
  })
  .expect_err("exhausted names must fail");

  assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
  assert_eq!(attempts, 8, "creation must stop at the retry budget");
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
