use super::*;

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

/// A real `/tmp` alias round-trips: symlink, marker, owner, removal.
#[test]
fn shared_temp_alias_and_marker_round_trip() {
  let alias_dir = Path::new("/tmp");
  let euid = current_euid();
  let Some(index) = (1..=9).rev().find(|index| {
    std::fs::symlink_metadata(socket_path(alias_dir, *index)).is_err()
      && std::fs::symlink_metadata(marker_path(alias_dir, euid, *index)).is_err()
  }) else {
    eprintln!("skipping /tmp alias round trip: every index 1..=9 is occupied");
    return;
  };

  let target_dir = scratch("shared-tmp");
  let (listener, target, target_id) = bind_target(&target_dir, index);
  let alias = socket_path(alias_dir, index);
  let marker = marker_path(alias_dir, euid, index);
  let PublishOutcome::Created(owned) = publish_alias(alias_dir, index, &target, target_id, euid)
  else {
    eprintln!("skipping /tmp alias round trip: index {index} was taken concurrently");
    drop(listener);
    let _ = std::fs::remove_dir_all(&target_dir);
    return;
  };
  // Best-effort cleanup if an assertion below panics in the shared root.
  struct TempEntries<'a> {
    alias: &'a Path,
    marker: &'a Path,
  }
  impl Drop for TempEntries<'_> {
    fn drop(&mut self) {
      let _ = std::fs::remove_file(self.alias);
      let _ = std::fs::remove_file(self.marker);
    }
  }
  let _cleanup = TempEntries {
    alias: &alias,
    marker: &marker,
  };

  assert_eq!(std::fs::read_link(&alias).expect("alias link"), target);
  let meta = std::fs::symlink_metadata(&marker).expect("marker metadata");
  assert!(meta.file_type().is_file(), "marker must be a file");
  assert_eq!(meta.mode() & 0o777, 0o600);
  assert_eq!(meta.uid(), euid);

  remove_owned_alias(&owned, euid);
  assert!(
    std::fs::symlink_metadata(&alias).is_err(),
    "owned alias must be removed"
  );
  assert!(
    std::fs::symlink_metadata(&marker).is_err(),
    "owned marker must be removed"
  );

  drop(listener);
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
