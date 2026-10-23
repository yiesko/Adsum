use super::*;

/// A probe-name collision retries the next name and removes only its own.
#[test]
fn probe_temp_write_retries_name_collisions() {
  let dir = scratch("probe-collision");
  let seq = AtomicU64::new(0);
  let occupied = dir.join(format!(".adsum-ipc-probe-{}-0", std::process::id()));
  std::fs::write(&occupied, b"occupied").expect("occupied probe");

  probe_temp_write_with(&dir, &seq).expect("second name must be used");

  assert_eq!(
    seq.load(Ordering::Relaxed),
    2,
    "both attempts must advance the counter"
  );
  assert!(occupied.is_file(), "the occupied name must be preserved");
  assert_eq!(
    std::fs::read(&occupied).expect("occupied content"),
    b"occupied"
  );
  assert_eq!(
    std::fs::read_dir(&dir).expect("scratch listing").count(),
    1,
    "the probe file must be removed"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// Eight collisions exhaust the probe-name budget with a clear error.
#[test]
fn probe_temp_write_exhausts_after_eight_collisions() {
  let dir = scratch("probe-exhausted");
  let seq = AtomicU64::new(0);
  for name in 0..8 {
    let occupied = dir.join(format!(".adsum-ipc-probe-{}-{name}", std::process::id()));
    std::fs::write(&occupied, b"occupied").expect("occupied probe");
  }

  let err = probe_temp_write_with(&dir, &seq).expect_err("names must be exhausted");

  assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
  assert!(
    err
      .to_string()
      .contains("temporary IPC probe name exhausted"),
    "unexpected error: {err}"
  );
  assert_eq!(
    seq.load(Ordering::Relaxed),
    8,
    "attempts must stop at eight"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// Errors other than a collision propagate without consuming more names.
#[test]
fn probe_temp_write_propagates_non_collision_errors() {
  let dir = scratch("probe-error");
  let seq = AtomicU64::new(0);
  let missing = dir.join("missing");

  let err = probe_temp_write_with(&missing, &seq).expect_err("missing dir must fail");

  assert_eq!(err.kind(), io::ErrorKind::NotFound);
  assert_eq!(seq.load(Ordering::Relaxed), 1, "errors must not retry");
  let _ = std::fs::remove_dir_all(&dir);
}
