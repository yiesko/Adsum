use super::*;

/// Recovery leaves a malformed marker and its alias untouched.
#[test]
fn recover_alias_preserves_malformed_marker() {
  let alias_dir = scratch("recover-malformed");
  let target_dir = scratch("recover-malformed-target");
  let euid = current_euid();
  let index = 6;
  let (listener, target, _target_id) = bind_target(&target_dir, index);
  let alias = socket_path(&alias_dir, index);
  std::os::unix::fs::symlink(&target, &alias).expect("alias symlink");
  let marker = marker_path(&alias_dir, euid, index);
  write_raw_marker(&marker, br#"{"version":1"#);

  assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Conflict);
  assert_eq!(std::fs::read_link(&alias).expect("alias kept"), target);
  assert!(
    std::fs::symlink_metadata(&marker).is_ok(),
    "invalid marker must stay"
  );

  drop(listener);
  let _ = std::fs::remove_dir_all(&alias_dir);
  let _ = std::fs::remove_dir_all(&target_dir);
}

/// A symlinked alias without a marker is preserved, never claimed.
#[test]
fn recovery_preserves_link_without_marker() {
  let alias_dir = scratch("recover-link-no-marker");
  let target_dir = scratch("recover-link-no-marker-target");
  let euid = current_euid();
  let index = 7;
  let (listener, target, _target_id) = bind_target(&target_dir, index);
  let alias = socket_path(&alias_dir, index);
  std::os::unix::fs::symlink(&target, &alias).expect("alias symlink");

  assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Preserved);
  assert_eq!(std::fs::read_link(&alias).expect("alias kept"), target);
  assert!(
    std::fs::symlink_metadata(marker_path(&alias_dir, euid, index)).is_err(),
    "recovery must not create a marker"
  );

  drop(listener);
  let _ = std::fs::remove_dir_all(&alias_dir);
  let _ = std::fs::remove_dir_all(&target_dir);
}

/// A validated marker with no alias and a missing target is removed.
#[test]
fn recovery_removes_marker_when_target_missing_and_alias_absent() {
  let alias_dir = scratch("recover-missing-target");
  let target_dir = scratch("recover-missing-target-target");
  let euid = current_euid();
  let index = 8;
  let marker = publish(&alias_dir, &target_dir, index);
  std::fs::remove_file(socket_path(&target_dir, index)).expect("remove target");

  assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Absent);
  assert!(
    std::fs::symlink_metadata(&marker.path).is_err(),
    "orphan marker must be removed"
  );

  let _ = std::fs::remove_dir_all(&alias_dir);
  let _ = std::fs::remove_dir_all(&target_dir);
}

/// A stale target path rebound to a live listener keeps alias and marker.
#[test]
fn recovery_preserves_alias_when_target_is_rebound_live() {
  let alias_dir = scratch("recover-rebound");
  let target_dir = scratch("recover-rebound-target");
  let euid = current_euid();
  let index = 9;
  let (listener, target, target_id) = bind_target(&target_dir, index);
  let owned = published_owned(&alias_dir, index, &target, target_id);
  drop(listener);
  std::fs::remove_file(&target).expect("remove stale target path");
  let rebound = UnixListener::bind(&target).expect("rebound live target");

  assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Preserved);
  assert_eq!(
    std::fs::read_link(&owned.alias).expect("alias kept"),
    target
  );
  assert!(
    std::fs::symlink_metadata(&owned.marker.path).is_ok(),
    "marker must stay with the live rebound target"
  );

  drop(rebound);
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

/// A stale target with a different inode is not the published object;
/// recovery preserves the alias and its marker.
#[test]
fn recovery_preserves_alias_when_stale_target_identity_differs() {
  let alias_dir = scratch("recover-stale-identity");
  let target_dir = scratch("recover-stale-identity-target");
  let euid = current_euid();
  let index = 0;
  let (listener, target, target_id) = bind_target(&target_dir, index);
  let owned = published_owned(&alias_dir, index, &target, target_id);
  drop(listener);

  let replacement_path = target_dir.join("replacement");
  let replacement = UnixListener::bind(&replacement_path).expect("replacement listener");
  let replacement_id = file_id(&replacement_path);
  assert_ne!(
    replacement_id, target_id,
    "replacement must have a distinct inode"
  );
  std::fs::remove_file(&target).expect("remove stale target path");
  std::fs::rename(&replacement_path, &target).expect("move replacement into place");
  drop(replacement);

  assert_eq!(probe_socket(&target).state, SocketState::Stale);
  assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Preserved);
  assert_eq!(
    std::fs::read_link(&owned.alias).expect("alias kept"),
    target
  );
  assert!(
    std::fs::symlink_metadata(&owned.marker.path).is_ok(),
    "marker must stay with a foreign stale target"
  );

  let _ = std::fs::remove_dir_all(&alias_dir);
  let _ = std::fs::remove_dir_all(&target_dir);
}

/// A target replaced by a regular file is preserved as a foreign entry.
#[test]
fn recovery_preserves_alias_when_target_is_not_a_socket() {
  let alias_dir = scratch("recover-not-socket");
  let target_dir = scratch("recover-not-socket-target");
  let euid = current_euid();
  let index = 0;
  let (listener, target, target_id) = bind_target(&target_dir, index);
  let owned = published_owned(&alias_dir, index, &target, target_id);
  drop(listener);
  std::fs::remove_file(&target).expect("remove stale target path");
  std::fs::write(&target, b"foreign").expect("replace with regular file");

  assert_eq!(probe_socket(&target).state, SocketState::NotSocket);
  assert_eq!(recover_alias(&alias_dir, index, euid), Recovery::Preserved);
  assert_eq!(
    std::fs::read_link(&owned.alias).expect("alias kept"),
    target
  );
  assert!(
    std::fs::symlink_metadata(&owned.marker.path).is_ok(),
    "marker must stay with a foreign non-socket target"
  );
  assert_eq!(
    std::fs::read(&target).expect("foreign file kept"),
    b"foreign"
  );

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

/// Scanning alias dirs removes orphan markers and keeps foreign entries.
#[test]
fn recover_orphan_aliases_removes_only_orphan_markers() {
  let alias_dir = scratch("orphan-scan");
  let target_dir = scratch("orphan-scan-target");
  let euid = current_euid();
  let (listener, target, target_id) = bind_target(&target_dir, 0);
  let alias = socket_path(&alias_dir, 0);
  let marker =
    publish_marker(&alias_dir, &alias, &target, target_id, euid, 0).expect("publish orphan marker");
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
