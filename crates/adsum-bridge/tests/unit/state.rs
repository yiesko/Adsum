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
