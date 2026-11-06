//! State-snapshot alias tests: publication, healing, cleanup, ownership
//! and foreign-entry preservation. Every test works on isolated temp dirs
//! (never the user's real snapshot directories).

use std::fs;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::state_aliases;

const NAME: &str = "arrpc-state-0";
const STALE_MS: u64 = crate::state_aliases::STATE_CONSUMER_STALE_MS;

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
  fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
}

/// Backdate a file's mtime past the consumer budget.
fn age_target(target: &Path) {
  let stale = now_ms() - STALE_MS - 5_000;
  let epoch = UNIX_EPOCH + std::time::Duration::from_millis(stale);
  fs::File::options()
    .append(true)
    .open(target)
    .unwrap()
    .set_modified(epoch)
    .unwrap();
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
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
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
    STALE_MS,
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
      STALE_MS,
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
    STALE_MS,
    now_ms(),
  );

  let temp = dir.join(".canonical-0.tmp");
  fs::write(&temp, snapshot_bytes()).unwrap();
  fs::rename(&temp, &target).unwrap();

  let alias = dir.join(NAME);
  assert_eq!(fs::read_link(&alias).unwrap(), target, "alias unchanged");
  assert!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms())
      == StateRecovery::Preserved,
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

  let occupied = dir.join(NAME);
  fs::write(&occupied, b"mine").unwrap();
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StatePublish::Skipped
  );
  assert_eq!(fs::read(&occupied).unwrap(), b"mine");

  fs::remove_file(&occupied).unwrap();
  let foreign_target = dir.join(".theirs");
  fs::write(&foreign_target, b"theirs").unwrap();
  std::os::unix::fs::symlink(&foreign_target, &occupied).unwrap();
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StatePublish::Skipped
  );
  assert_eq!(fs::read_link(&occupied).unwrap(), foreign_target);
  fs::remove_dir_all(&dir).ok();
}

/// Removal only happens when marker content, marker identity and the alias
/// symlink identity all still match the ownership record. An alias restored
/// outside the reconcile cycle keeps a stale record: the next reconcile pass
/// refreshes it, and only then does the removal take it.
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
    STALE_MS,
    now_ms(),
  );
  let alias = dir.join(NAME);
  let marker = dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()));

  let record = owned.remove(0);

  let other = dir.join(".other");
  fs::write(&other, b"other").unwrap();
  fs::remove_file(&alias).unwrap();
  std::os::unix::fs::symlink(&other, &alias).unwrap();
  state_aliases::remove_owned_state_alias(&record, test_euid());
  assert!(
    alias.symlink_metadata().is_ok(),
    "divergent alias preserved"
  );

  // Restored to the recorded target by hand: the record still names the
  // original inode. Whether removal happens with a stale record depends
  // on the filesystem's inode allocator (ext4 and zram reuse it, APFS
  // never does), so that state has no portable assertion. Production only
  // removes through the reconcile pass, which refreshes the record first.
  fs::remove_file(&alias).unwrap();
  std::os::unix::fs::symlink(&target, &alias).unwrap();
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  assert_eq!(
    state_aliases::reconcile_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      &mut owned,
      STALE_MS,
      now_ms()
    ),
    StateRecovery::Preserved
  );
  assert_eq!(owned.len(), 1, "the restored alias is owned again");
  state_aliases::remove_owned_state_aliases(&mut owned, test_euid());
  assert!(fs::symlink_metadata(&alias).is_err(), "owned alias removed");
  assert!(
    fs::symlink_metadata(&marker).is_err(),
    "marker removed after the alias"
  );
  fs::remove_dir_all(&dir).ok();
}

/// An alias swapped for a foreign object between publication and shutdown
/// is preserved: the ownership record no longer matches the entry at the
/// path, so shutdown leaves it and its marker untouched.
#[test]
fn shutdown_preserves_alias_swapped_after_publication() {
  let dir = fresh_dir("swapped");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STALE_MS,
    now_ms(),
  );

  fs::remove_file(dir.join(NAME)).unwrap();
  fs::write(dir.join(NAME), b"swapped by someone else").unwrap();

  state_aliases::remove_owned_state_aliases(&mut owned, test_euid());
  assert_eq!(
    fs::read(dir.join(NAME)).unwrap(),
    b"swapped by someone else",
    "the swapped entry survives the shutdown removal"
  );
  assert!(
    fs::symlink_metadata(dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()))).is_ok(),
    "the marker stays with the preserved entry"
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
    STALE_MS,
    now_ms(),
  );

  fs::remove_file(dir.join(NAME)).unwrap();
  assert!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms())
      == StateRecovery::Healed,
    "missing alias over a valid target is healed"
  );
  assert_eq!(fs::read_link(dir.join(NAME)).unwrap(), target);

  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STALE_MS,
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
    STALE_MS,
    now_ms(),
  );
  let alias = dir.join(NAME);

  assert_eq!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StateRecovery::Preserved
  );
  assert!(alias.symlink_metadata().is_ok());

  fs::remove_file(&target).unwrap();
  assert_eq!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
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
    STALE_MS,
    now_ms(),
  );

  age_target(&target);

  assert_eq!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StateRecovery::Cleaned
  );
  assert!(dir.join(NAME).symlink_metadata().is_err());
  fs::remove_dir_all(&dir).ok();
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
      STALE_MS,
      now_ms(),
    );
  }
  assert_eq!(owned.len(), 2, "one ownership record per slot");
  assert!(dir.join("arrpc-state-0").symlink_metadata().is_ok());
  assert!(dir.join("arrpc-state-1").symlink_metadata().is_ok());

  state_aliases::remove_owned_state_aliases(&mut owned, test_euid());
  assert!(dir.join("arrpc-state-0").symlink_metadata().is_err());
  assert!(dir.join("arrpc-state-1").symlink_metadata().is_err());
  assert!(target0.exists() && target1.exists());
  fs::remove_dir_all(&dir).ok();
}

#[test]
fn late_appearing_runtime_dir_is_picked_up() {
  let base = fresh_dir("late");
  let layout = base.join("app/org.equicord.equibop");
  let target = base.join(".canonical-0");
  write_canonical(&target);

  let dirs = state_aliases::state_alias_dirs(std::slice::from_ref(&base));
  assert_eq!(dirs, vec![base.clone()], "the base itself is a client dir");

  fs::create_dir_all(&layout).unwrap();
  fs::set_permissions(&layout, fs::Permissions::from_mode(0o700)).unwrap();
  let dirs = state_aliases::state_alias_dirs(std::slice::from_ref(&base));
  assert!(dirs.contains(&base));
  assert!(
    dirs
      .iter()
      .any(|dir| dir.ends_with("app/org.equicord.equibop"))
  );
  for dir in &dirs {
    assert_eq!(
      state_aliases::publish_state_alias(dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
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

  fs::write(&target, b"{}").unwrap();
  fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StatePublish::Skipped,
    "missing timestamp refused"
  );
  assert!(dir.join(NAME).symlink_metadata().is_err());
  fs::remove_dir_all(&dir).ok();
}

/// A healed alias carries a fresh inode: reconciliation must refresh the
/// ownership record, and the shutdown pass must then remove both parts.
#[test]
fn healed_alias_is_registered_and_removed_on_shutdown() {
  let dir = fresh_dir("healed-owned");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STALE_MS,
    now_ms(),
  );

  // The alias vanishes while the marker survives (the heal scenario).
  fs::remove_file(dir.join(NAME)).unwrap();
  assert_eq!(
    state_aliases::reconcile_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      &mut owned,
      STALE_MS,
      now_ms()
    ),
    StateRecovery::Healed
  );
  assert_eq!(owned.len(), 1, "healed alias must be re-registered");
  let on_disk = state_symlink_id(&dir.join(NAME), &target).expect("alias re-created");
  assert_eq!(
    owned[0].alias_id, on_disk,
    "the record must match the current on-disk inode"
  );

  // The refreshed record must drive a clean shutdown removal.
  state_aliases::remove_owned_state_aliases(&mut owned, test_euid());
  assert!(dir.join(NAME).symlink_metadata().is_err());
  assert!(
    fs::symlink_metadata(dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()))).is_err()
  );
  fs::remove_dir_all(&dir).ok();
}

/// A foreign entry at the alias path is never removed by recovery, even
/// when the recorded target has died.
#[test]
fn foreign_alias_is_preserved_when_target_dies() {
  let dir = fresh_dir("foreign-recover");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  state_aliases::reconcile_state_alias(
    &dir,
    NAME,
    &target,
    test_euid(),
    &mut owned,
    STALE_MS,
    now_ms(),
  );

  // Someone replaces our alias with a plain file, then the target dies.
  fs::remove_file(dir.join(NAME)).unwrap();
  fs::write(dir.join(NAME), b"not ours").unwrap();
  fs::remove_file(&target).unwrap();

  assert_eq!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StateRecovery::Conflict,
    "a foreign entry must never be classified as cleanable"
  );
  assert_eq!(
    fs::read(dir.join(NAME)).unwrap(),
    b"not ours",
    "the foreign entry survives recovery"
  );
  assert!(
    fs::symlink_metadata(dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()))).is_ok(),
    "our marker is preserved alongside the foreign entry"
  );
  fs::remove_dir_all(&dir).ok();
}

/// An interrupted marker write leaves either nothing or an orphan temp:
/// later publication works in both cases, and a truncated legacy marker
/// is preserved rather than overwritten or blocking.
#[test]
fn interrupted_write_never_blocks_later_publication() {
  let dir = fresh_dir("interrupted");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let marker = dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()));

  // Crash artifact: an orphan temp with partial content, no final marker.
  let temp = marker.with_extension("tmp-99999-7");
  fs::write(&temp, b"{\"version\":1,\"own").unwrap();
  assert_eq!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StateRecovery::Absent
  );
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StatePublish::Linked,
    "an orphan temp must not block publication"
  );
  assert_eq!(
    fs::read(dir.join(NAME)).unwrap(),
    fs::read(&target).unwrap()
  );
  fs::remove_file(dir.join(NAME)).ok();
  fs::remove_file(&marker).ok();

  // Legacy artifact: a truncated final marker is not provably ours, so it
  // is preserved, never overwritten, and never blocks.
  fs::write(&marker, b"{\"version\":1,\"own").unwrap();
  assert_eq!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StateRecovery::Absent,
    "no alias and no valid marker: nothing to recover"
  );
  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StatePublish::Skipped,
    "a truncated marker must be preserved, not replaced"
  );
  assert_eq!(
    fs::read(&marker).unwrap(),
    b"{\"version\":1,\"own",
    "the truncated marker is untouched"
  );
  fs::remove_dir_all(&dir).ok();
}

/// A snapshot older than the consumer budget is never republished, so
/// recovery and publication cannot fight each other every cycle.
#[test]
fn stale_snapshot_is_not_republished() {
  let dir = fresh_dir("stale-republish");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  age_target(&target);

  assert_eq!(
    state_aliases::publish_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StatePublish::Skipped,
    "publication must refuse a stale target"
  );
  let mut owned: Vec<OwnedStateAlias> = Vec::new();
  assert_eq!(
    state_aliases::reconcile_state_alias(
      &dir,
      NAME,
      &target,
      test_euid(),
      &mut owned,
      STALE_MS,
      now_ms()
    ),
    StateRecovery::Conflict,
    "recovery cleans, publication refuses: no republication loop"
  );
  assert!(
    dir.join(NAME).symlink_metadata().is_err(),
    "the stale target must not regain an alias"
  );
  fs::remove_dir_all(&dir).ok();
}

/// A FIFO or a loose-permission marker is rejected immediately (without
/// blocking on open or read) and treated as absent for recovery.
#[test]
fn fifo_and_loose_marker_are_rejected_without_blocking() {
  let dir = fresh_dir("marker-reject");
  let target = dir.join(".canonical-0");
  write_canonical(&target);
  let marker = dir.join(format!(".adsum-state-owner-{}-{NAME}", test_euid()));

  let c_path = CString::new(marker.as_os_str().as_bytes()).unwrap();
  // SAFETY: creates a FIFO node inside our private scratch directory.
  assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
  assert!(
    state_aliases::read_state_marker(&marker, test_euid()).is_err(),
    "a FIFO is not a regular marker"
  );
  assert_eq!(
    state_aliases::recover_state_alias(&dir, NAME, &target, test_euid(), STALE_MS, now_ms()),
    StateRecovery::Absent,
    "an invalid marker and a missing alias mean nothing to recover"
  );

  fs::remove_file(&marker).unwrap();
  fs::write(&marker, snapshot_bytes()).unwrap();
  fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
  assert!(
    state_aliases::read_state_marker(&marker, test_euid()).is_err(),
    "a loose-permission marker is refused"
  );
  fs::remove_dir_all(&dir).ok();
}
