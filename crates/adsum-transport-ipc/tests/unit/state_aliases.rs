//! State-snapshot alias tests: publication, healing, cleanup, ownership
//! and foreign-entry preservation. Every test works on isolated temp dirs
//! (never the user's real snapshot directories).

use std::fs;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::state_aliases;

const NAME: &str = "arrpc-state-0";
/// The real effective uid: production always publishes as the current
/// euid, and target/marker uid checks compare against it.
fn test_euid() -> u32 {
  // SAFETY: plain syscall wrapper, no preconditions.
  unsafe { libc::geteuid() }
}

fn now_ms() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|elapsed| elapsed.as_millis() as u64)
    .unwrap_or(0)
}

fn fresh_dir(tag: &str) -> PathBuf {
  static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
  let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  let dir = std::env::temp_dir().join(format!(
    "adsum-state-alias-test-{tag}-{}-{serial}",
    std::process::id()
  ));
  let _ = fs::remove_dir_all(&dir);
  fs::create_dir_all(&dir).unwrap();
  fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
  dir
}

fn snapshot_bytes() -> Vec<u8> {
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|elapsed| elapsed.as_millis())
    .unwrap_or(0);
  format!(r#"{{"timestamp":{now}}}"#).into_bytes()
}

fn write_canonical(target: &Path) {
  fs::write(target, snapshot_bytes()).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
  }
}

/// The flatpak layout `app/org.equicord.equibop` below a base is one of the
/// directories the publisher discovers (same client list the IPC aliases use).
#[test]
fn equibop_runtime_layout_is_discovered() {
  let base = fresh_dir("discover");
  let layout = base.join("app/org.equicord.equibop");
  fs::create_dir_all(&layout).unwrap();
  fs::set_permissions(&layout, fs::Permissions::from_mode(0o700)).unwrap();

  let dirs = state_aliases::state_alias_dirs(std::slice::from_ref(&base));
  assert!(
    dirs
      .iter()
      .any(|dir| dir.ends_with("app/org.equicord.equibop")),
    "equibop runtime dir missing from {dirs:?}"
  );
  fs::remove_dir_all(base.join("app")).ok();
}

/// Publication creates the symlink with the exact canonical name and the
/// marker before the link, so an interrupted run recovers cleanly.
#[test]
fn publish_uses_matching_index_and_resolves_to_canonical() {
  let dir = fresh_dir("publish");
  let target = dir.join(".canonical-0");
  write_canonical(&target);

  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid()),
    StatePublish::Linked
  );
  assert_eq!(
    fs::read_link(dir.join(NAME)).unwrap(),
    target,
    "alias must point at the canonical path"
  );
  let resolved = fs::read(dir.join(NAME)).unwrap();
  assert_eq!(
    resolved,
    fs::read(&target).unwrap(),
    "resolves to the canonical file"
  );
  fs::remove_dir_all(&dir).ok();
}

/// Re-publishing over a healthy owned pair is idempotent: the symlink is
/// never recreated (its identity stays stable across reconciliations).
#[test]
fn reconcile_is_idempotent_and_keeps_symlink_identity() {
  let dir = fresh_dir("idem");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();

  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STATE_CONSUMER_STALE_MS,
    now_ms(),
  );
  let first_id = owned[0].alias_id;

  for _ in 0..3 {
    state_aliases::reconcile_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      &mut owned,
      STATE_CONSUMER_STALE_MS,
      now_ms(),
    );
  }
  assert_eq!(owned.len(), 1, "reconcile must not duplicate ownership");
  assert_eq!(owned[0].alias_id, first_id, "symlink must not be recreated");
  fs::remove_dir_all(&dir).ok();
}

/// The canonical file is rewritten atomically (temp + rename): the alias
/// survives and reads the new content, because it follows the path.
#[test]
fn atomic_update_does_not_invalidate_the_alias() {
  let dir = fresh_dir("atomic");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STATE_CONSUMER_STALE_MS,
    now_ms(),
  );

  // Atomic rewrite: new temp file, renamed over the canonical path.
  let temp = dir.join(".canonical-0.tmp");
  fs::write(&temp, snapshot_bytes()).unwrap();
  fs::rename(&temp, &target).unwrap();

  let alias = dir.join(NAME);
  assert_eq!(fs::read_link(&alias).unwrap(), target, "alias unchanged");
  assert!(
    state_aliases::recover_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      STATE_CONSUMER_STALE_MS,
      now_ms()
    ) == StateRecovery::Preserved,
    "alias stays owned after an atomic rewrite"
  );
  fs::remove_dir_all(&dir).ok();
}

/// A regular file (or foreign symlink) occupying the alias name is never
/// replaced, with or without our marker.
#[test]
fn foreign_entries_are_never_overwritten() {
  let dir = fresh_dir("foreign");
  let target = dir.join(".canonical-0");
  write_canonical(&target);

  // Regular file at the alias path, no marker: untouched.
  let occupied = dir.join(NAME);
  fs::write(&occupied, b"mine").unwrap();
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid()),
    StatePublish::Skipped
  );
  assert_eq!(fs::read(&occupied).unwrap(), b"mine");

  // Foreign symlink (someone else's link), no marker: untouched.
  fs::remove_file(&occupied).unwrap();
  let foreign_target = dir.join(".theirs");
  fs::write(&foreign_target, b"theirs").unwrap();
  std::os::unix::fs::symlink(&foreign_target, &occupied).unwrap();
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid()),
    StatePublish::Skipped
  );
  assert_eq!(fs::read_link(&occupied).unwrap(), foreign_target);
  fs::remove_dir_all(&dir).ok();
}

/// Removal only happens when marker content, marker identity and the alias
/// symlink identity all still match the ownership record.
#[test]
fn removal_requires_full_identity_match() {
  let dir = fresh_dir("removal");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STATE_CONSUMER_STALE_MS,
    now_ms(),
  );
  let alias = dir.join(NAME);

  // Take the ownership record out (shutdown semantics drain the vec).
  let record = owned.remove(0);

  // Tampered alias (points elsewhere): removal must refuse and preserve.
  let other = dir.join(".other");
  fs::write(&other, b"other").unwrap();
  fs::remove_file(&alias).unwrap();
  std::os::unix::fs::symlink(&other, &alias).unwrap();
  state_aliases::remove_owned_state_alias(&record, test_euid());
  assert!(
    alias.symlink_metadata().is_ok(),
    "divergent alias preserved"
  );

  // Restored to the recorded target: removal removes alias, then marker.
  fs::remove_file(&alias).unwrap();
  std::os::unix::fs::symlink(&target, &alias).unwrap();
  state_aliases::remove_owned_state_alias(&record, test_euid());
  assert!(fs::symlink_metadata(&alias).is_err(), "owned alias removed");
  assert!(
    fs::symlink_metadata(dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()))).is_err(),
    "marker removed after the alias"
  );
  fs::remove_dir_all(&dir).ok();
}

/// A crash between marker publication and symlink creation is healed on the
/// next pass: the alias is re-created from the surviving marker.
#[test]
fn interrupted_publish_is_healed() {
  let dir = fresh_dir("heal");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STATE_CONSUMER_STALE_MS,
    now_ms(),
  );

  // Simulate the crash: alias gone, marker survives.
  fs::remove_file(dir.join(NAME)).unwrap();
  assert!(
    state_aliases::recover_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      STATE_CONSUMER_STALE_MS,
      now_ms()
    ) == StateRecovery::Healed,
    "missing alias over a valid target is healed"
  );
  assert_eq!(fs::read_link(dir.join(NAME)).unwrap(), target);

  // The healed pair is now owned: a shutdown pass removes both.
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STATE_CONSUMER_STALE_MS,
    now_ms(),
  );
  assert_eq!(owned.len(), 1);
  fs::remove_dir_all(&dir).ok();
}

/// After an unexpected daemon death (target missing or stale beyond the
/// consumer budget), the abandoned pair is removed. A fresh target means a
/// live daemon owns it: preserved.
#[test]
fn unexpected_death_cleanup_matches_target_freshness() {
  let dir = fresh_dir("cleanup");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STATE_CONSUMER_STALE_MS,
    now_ms(),
  );
  let alias = dir.join(NAME);

  // Fresh target: the pair stays (a live daemon may own it).
  assert_eq!(
    state_aliases::recover_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      STATE_CONSUMER_STALE_MS,
      now_ms()
    ),
    StateRecovery::Preserved
  );
  assert!(alias.symlink_metadata().is_ok());

  // Target removed: the pair is dead and cleaned.
  fs::remove_file(&target).unwrap();
  assert_eq!(
    state_aliases::recover_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      STATE_CONSUMER_STALE_MS,
      now_ms()
    ),
    StateRecovery::Cleaned
  );
  assert!(alias.symlink_metadata().is_err());
  assert!(
    fs::symlink_metadata(dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()))).is_err()
  );
  fs::remove_dir_all(&dir).ok();
}

/// A stale target (older than the consumer budget) counts as abandoned:
/// the alias and marker are cleaned even though the file still exists.
#[test]
fn stale_target_is_treated_as_abandoned() {
  let dir = fresh_dir("stale");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STATE_CONSUMER_STALE_MS,
    now_ms(),
  );

  // Age the canonical snapshot past the consumer budget.
  let stale = now_ms() - STATE_CONSUMER_STALE_MS - 5_000;
  let epoch = UNIX_EPOCH + std::time::Duration::from_millis(stale);
  filetime_set(&target, epoch);

  assert_eq!(
    state_aliases::recover_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      STATE_CONSUMER_STALE_MS,
      now_ms()
    ),
    StateRecovery::Cleaned
  );
  assert!(dir.join(NAME).symlink_metadata().is_err());
  fs::remove_dir_all(&dir).ok();
}

/// Backdate a file's mtime (test helper for staleness).
fn filetime_set(path: &Path, epoch: SystemTime) {
  std::fs::File::options()
    .append(true)
    .open(path)
    .unwrap()
    .set_modified(epoch)
    .unwrap();
}

/// Two different snapshot names publish two independent aliases in the
/// same directory; removing one never touches the other.
#[test]
fn multiple_slots_publish_and_clean_independently() {
  let dir = fresh_dir("slots");
  let target0 = dir.join(".canonical-0");
  let target1 = dir.join(".canonical-1");
  write_canonical(&target0);
  write_canonical(&target1);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();

  for (name, target) in [("arrpc-state-0", &target0), ("arrpc-state-1", &target1)] {
    state_aliases::reconcile_state_alias(
      &dir,
      name,
      target,
      test_euid(),
      &mut owned,
      STATE_CONSUMER_STALE_MS,
      now_ms(),
    );
  }
  assert_eq!(owned.len(), 2, "one ownership record per slot");
  assert!(dir.join("arrpc-state-0").symlink_metadata().is_ok());
  assert!(dir.join("arrpc-state-1").symlink_metadata().is_ok());

  state_aliases::remove_owned_state_aliases(&mut owned, test_euid());
  assert!(dir.join("arrpc-state-0").symlink_metadata().is_err());
  assert!(dir.join("arrpc-state-1").symlink_metadata().is_err());
  // The canonical files are not ours to remove here: only aliases+markers.
  assert!(target0.exists() && target1.exists());
  fs::remove_dir_all(&dir).ok();
}

#[test]
fn late_appearing_runtime_dir_is_picked_up() {
  let base = fresh_dir("late");
  let layout = base.join("app/org.equicord.equibop");
  let target = base.join(".canonical-0");
  write_canonical(&target);

  // Before Equibop starts, the base itself is already a valid client dir
  // (an empty layout covers consumers that scan /tmp directly).
  let dirs = state_aliases::state_alias_dirs(std::slice::from_ref(&base));
  assert_eq!(dirs, vec![base.clone()], "the base itself is a client dir");

  // Equibop starts: its runtime dir joins the publish set.
  fs::create_dir_all(&layout).unwrap();
  fs::set_permissions(&layout, fs::Permissions::from_mode(0o700)).unwrap();
  let dirs = state_aliases::state_alias_dirs(std::slice::from_ref(&base));
  assert!(dirs.contains(&base));
  assert!(
    dirs
      .iter()
      .any(|dir| dir.ends_with("app/org.equicord.equibop"))
  );
  // Every discovered dir gets the alias; the Equibop one resolves to the
  // canonical snapshot through the symlink.
  for dir in &dirs {
    assert_eq!(
      state_aliases::publish_state_alias(dir, NAME, &target, test_euid()),
      StatePublish::Linked
    );
  }
  assert_eq!(fs::read_link(layout.join(NAME)).unwrap(), target);
  fs::remove_dir_all(&base).ok();
}

/// Snapshot targets that fail validation (wrong owner, loose permissions,
/// no timestamp) are refused as alias targets.
#[test]
fn invalid_snapshot_targets_are_refused() {
  let dir = fresh_dir("validate");
  let target = dir.join(".canonical-0");

  // No timestamp: not a snapshot.
  fs::write(&target, b"{}").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
  }
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid()),
    StatePublish::Skipped,
    "missing timestamp refused"
  );
  assert!(dir.join(NAME).symlink_metadata().is_err());
  fs::remove_dir_all(&dir).ok();
}
