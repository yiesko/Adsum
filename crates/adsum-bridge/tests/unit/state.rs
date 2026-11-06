use super::*;

/// A planted symlink at the temp path is never followed: the target
/// keeps its content. Same-uid plants are reclaimed (link removed,
/// fresh file created); other-uid plants cannot even be removed under
/// the sticky bit, so the write fails closed instead.
#[cfg(unix)]
#[test]
fn tmp_symlink_is_never_followed() {
  use std::os::unix::fs::symlink;

  let dir = std::env::temp_dir().join(format!("adsum-state-test-tmplink-{}", std::process::id()));
  let _ = std::fs::create_dir_all(&dir);
  let target = dir.join("canary");
  std::fs::write(&target, b"untouched").expect("canary");
  let tmp = dir.join("adsum-state-0.tmp-1-1");
  symlink(&target, &tmp).expect("plant link");

  let _ = open_snapshot_tmp(&tmp);
  assert_eq!(std::fs::read(&target).expect("canary reads"), b"untouched");
  assert!(
    !std::fs::symlink_metadata(&tmp)
      .expect("tmp meta")
      .is_symlink(),
    "link must be gone or never created through"
  );

  let _ = std::fs::remove_dir_all(&dir);
}

/// A stale regular temp (crashed holder, recycled pid) is replaced:
/// remove-and-retry reclaims the name with a fresh owner-only file.
#[test]
fn stale_regular_tmp_is_reclaimed() {
  let dir = std::env::temp_dir().join(format!(
    "adsum-state-test-tmpreclaim-{}",
    std::process::id()
  ));
  let _ = std::fs::create_dir_all(&dir);
  let tmp = dir.join("adsum-state-0.tmp-1-1");
  std::fs::write(&tmp, b"stale").expect("stale tmp");

  let mut file = open_snapshot_tmp(&tmp).expect("reclaims stale tmp");
  file.write_all(b"fresh").expect("write");
  drop(file);
  assert_eq!(std::fs::read(&tmp).expect("reads"), b"fresh");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&tmp).expect("meta").permissions().mode() & 0o777;
    assert_eq!(
      mode, 0o600,
      "reclaimed temp must be owner-only, got {mode:o}"
    );
  }

  let _ = std::fs::remove_dir_all(&dir);
}

/// The prefix override serves Equibop's scanner (`arrpc-state-*`), and
/// blank, separator-bearing or over-long values fall back to the default
/// so a crafted env value cannot escape the temp directory.
#[test]
fn snapshot_prefix_overrides_safely() {
  assert_eq!(snapshot_prefix_from(None), "adsum-state-");
  assert_eq!(snapshot_prefix_from(Some("")), "adsum-state-");
  assert_eq!(snapshot_prefix_from(Some("adsum-state-")), "adsum-state-");
  assert_eq!(snapshot_prefix_from(Some("arrpc-state-")), "arrpc-state-");
  assert_eq!(snapshot_prefix_from(Some("a/b")), "adsum-state-");
  assert_eq!(snapshot_prefix_from(Some("..")), "adsum-state-");
  let long = "a".repeat(65);
  assert_eq!(snapshot_prefix_from(Some(&long)), "adsum-state-");
}

/// Slot selection honors the caller-supplied prefix: with Equibop's
/// `arrpc-state-` the chosen (and written) slots live in that namespace,
/// and the default-prefix namespace is untouched.
#[test]
fn select_slot_honors_custom_prefix() {
  let dir = std::env::temp_dir().join(format!("adsum-state-test-prefix-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  let now = now_secs();

  let slot = select_slot_with_prefix(&dir, now, "arrpc-state-");
  assert_eq!(slot, Some(dir.join("arrpc-state-0")));

  // A fresh, valid snapshot owns slot 0 (a corrupt file would be treated
  // as reusable), pushing selection to slot 1.
  std::fs::write(
    dir.join("arrpc-state-0"),
    format!(r#"{{"timestamp":{}}}"#, now.saturating_mul(1000)),
  )
  .unwrap();
  let slot = select_slot_with_prefix(&dir, now, "arrpc-state-");
  assert_eq!(slot, Some(dir.join("arrpc-state-1")));

  // The default-prefix namespace is untouched by the custom-prefix scan.
  let slot = select_slot_with_prefix(&dir, now, "adsum-state-");
  assert_eq!(slot, Some(dir.join("adsum-state-0")));
  std::fs::remove_dir_all(&dir).ok();
}
