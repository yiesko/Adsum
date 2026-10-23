use super::*;

/// Conventional bases keep first-seen order and append `/tmp` last.
#[test]
fn candidate_dirs_from_orders_dedupes_and_appends_tmp() {
  let dirs = candidate_dirs_from([
    ("XDG_RUNTIME_DIR", Some("/run/user/1000/".into())),
    ("TMPDIR", Some("/run/user/1000".into())),
    ("TMP", Some("".into())),
    ("TEMP", Some("/var/tmp".into())),
  ]);
  assert_eq!(
    dirs,
    vec![
      PathBuf::from("/run/user/1000"),
      PathBuf::from("/var/tmp"),
      PathBuf::from("/tmp"),
    ]
  );

  let fallback = candidate_dirs_from([
    ("XDG_RUNTIME_DIR", None),
    ("TMPDIR", None),
    ("TMP", None),
    ("TEMP", None),
  ]);
  assert_eq!(fallback, vec![PathBuf::from("/tmp")]);
}

/// Existing bases resolve to their real path once; unsafe shapes stay raw.
#[test]
fn candidate_bases_are_canonicalized_and_unsafe_paths_are_untouched() {
  let real = scratch("canonical-base-real");
  let link_parent = scratch("canonical-base-link");
  let link = link_parent.join("runtime");
  std::os::unix::fs::symlink(&real, &link).expect("base symlink");

  let dirs = canonicalize_candidate_bases(vec![
    link,
    real.clone(),
    PathBuf::from("relative/runtime"),
    PathBuf::from("/tmp/../runtime"),
  ]);

  assert_eq!(
    dirs,
    vec![
      real.clone(),
      PathBuf::from("relative/runtime"),
      PathBuf::from("/tmp/../runtime"),
    ]
  );

  let _ = std::fs::remove_dir_all(&real);
  let _ = std::fs::remove_dir_all(&link_parent);
}

/// The Flatpak socket dir is derived only from a usable runtime base.
#[test]
fn preferred_socket_dir_accepts_absolute_runtime_dir() {
  assert_eq!(
    preferred_socket_dir_from(Some("/run/user/1000/")),
    Some(PathBuf::from("/run/user/1000/app/com.discordapp.Discord"))
  );
  assert_eq!(
    preferred_socket_dir_from(Some("/run/user/1000")),
    Some(PathBuf::from("/run/user/1000/app/com.discordapp.Discord"))
  );
  assert_eq!(
    preferred_socket_dir_from(Some("/run/user/1000/../1000")),
    Some(PathBuf::from("/run/user/1000/app/com.discordapp.Discord"))
  );
}

/// Relative or shared-temp runtime bases never get the preferred dir.
#[test]
fn preferred_socket_dir_rejects_relative_and_shared_temp() {
  for xdg in [
    None,
    Some(""),
    Some("relative/run"),
    Some("/"),
    Some("/tmp"),
    Some("/tmp/"),
    Some("//tmp"),
    Some("/tmp/."),
    Some("/tmp/../tmp"),
    Some("/var/tmp"),
    Some("/var/tmp/"),
    Some("/var/tmp/../tmp"),
  ] {
    assert_eq!(
      preferred_socket_dir_from(xdg),
      None,
      "unexpected preferred dir for {xdg:?}"
    );
  }
}

/// Fallback dir name is qualified by the effective uid.
#[test]
fn fallback_dir_name_is_uid_qualified() {
  assert_eq!(fallback_dir_name(1000), ".adsum-ipc-1000");
}

/// Bind order prefers the Flatpak dir, filters temp roots, and dedupes.
#[test]
fn bind_dirs_put_preferred_first_filter_shared_temp_and_dedupe() {
  let scratch_dir = scratch("bind-order");
  let preferred = PathBuf::from("/run/user/1000/app/com.discordapp.Discord");
  let fallback = scratch_dir.join(".adsum-ipc-1000");
  let dirs = bind_dirs_in_order(
    vec![
      preferred.clone(),
      PathBuf::from("/tmp"),
      scratch_dir.clone(),
      scratch_dir.clone(),
      PathBuf::from("/var/tmp"),
    ],
    Some(&preferred),
    Some(fallback.clone()),
  );

  assert_eq!(dirs.first(), Some(&preferred));
  assert!(dirs.contains(&scratch_dir));
  assert!(
    !dirs
      .iter()
      .any(|dir| dir == Path::new("/tmp") || dir == Path::new("/var/tmp")),
    "shared temp roots must be filtered: {dirs:?}"
  );
  assert_eq!(dirs.last(), Some(&fallback));

  let mut unique = dirs.clone();
  unique.sort();
  unique.dedup();
  assert_eq!(unique.len(), dirs.len(), "bind dirs must not repeat");

  let _ = std::fs::remove_dir_all(&scratch_dir);
}

/// The fallback dir under canonical `/tmp` is created 0700 and owned by us.
#[test]
fn prepare_fallback_dir_creates_private_dir_under_tmp() {
  // macOS `/tmp` is a symlink and production rejects symlinked chains, so
  // canonicalize once and reuse that root for every assertion below.
  let Ok(root) = std::fs::canonicalize("/tmp") else {
    return;
  };
  let euid = current_euid();
  let fallback = root.join(fallback_dir_name(euid));
  let _guard = FALLBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());

  if !fallback.exists() {
    let dir = prepare_fallback_dir(std::slice::from_ref(&root), euid)
      .expect("prepare fallback dir under /tmp");
    assert_eq!(dir, fallback);
    assert_eq!(dir.parent(), Some(root.as_path()));
    assert_ne!(dir, root, "the fallback must never be the shared root");
    let meta = std::fs::symlink_metadata(&dir).expect("fallback metadata");
    assert_eq!(meta.mode() & 0o777, 0o700);
    assert_eq!(meta.uid(), euid);
    validate_bind_dir(&dir).expect("fallback dir must satisfy the bind policy");
    let _ = std::fs::remove_dir(&dir);
  }

  let unique = root.join(format!(".adsum-ipc-test-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&unique);
  prepare_private_dir(&unique).expect("create unique private dir");
  let meta = std::fs::symlink_metadata(&unique).expect("unique metadata");
  assert_eq!(meta.mode() & 0o777, 0o700);
  assert_eq!(meta.uid(), euid);
  let _ = std::fs::remove_dir_all(&unique);
}

/// A fallback dir owned by another uid is refused and never deleted.
#[test]
fn foreign_owned_fallback_is_not_deleted() {
  if current_euid() != 0 {
    return;
  }
  let tmp = Path::new("/tmp");
  if !tmp.is_dir() {
    return;
  }
  let fallback = tmp.join(fallback_dir_name(current_euid()));
  let _guard = FALLBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
  if fallback.exists() {
    return;
  }

  std::fs::create_dir(&fallback).expect("seed fallback dir");
  std::fs::set_permissions(&fallback, Permissions::from_mode(0o700)).expect("chmod fallback");
  let path = CString::new(fallback.as_os_str().as_bytes()).expect("path without nul");
  // SAFETY: `path` is a NUL-terminated string naming the directory created
  // above; chown only changes its owner and never outlives the buffer.
  let chown_result = unsafe { libc::chown(path.as_ptr(), 65534, 65534) };
  assert_eq!(chown_result, 0, "seed foreign owner");

  assert!(
    prepare_fallback_dir(&[tmp.to_path_buf()], current_euid()).is_none(),
    "foreign-owned fallback must be refused"
  );
  assert!(
    fallback.exists(),
    "foreign-owned fallback must not be deleted"
  );

  // SAFETY: same valid NUL-terminated path; restoring ownership as root
  // lets the test remove the directory it created.
  let restore_result = unsafe { libc::chown(path.as_ptr(), 0, 0) };
  assert_eq!(restore_result, 0, "restore fallback owner");
  let _ = std::fs::remove_dir(&fallback);
}
