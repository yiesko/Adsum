use super::*;

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
  std::fs::set_permissions(&replacement, Permissions::from_mode(0o600)).expect("chmod replacement");
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
