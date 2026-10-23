use super::*;

/// A missing runtime base is refused instead of being created.
#[test]
fn prepare_socket_dir_refuses_missing_base() {
  let base = scratch("prepare-missing-base");
  std::fs::remove_dir_all(&base).expect("remove scratch");
  let err = prepare_socket_dir(&base.join("app/com.discordapp.Discord"))
    .expect_err("missing base must fail");
  assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
  assert!(!base.exists(), "runtime base must not be created");
}

/// A symlinked runtime base is refused instead of followed.
#[test]
fn prepare_socket_dir_refuses_symlinked_base() {
  let real = scratch("prepare-base-real");
  let link_parent = scratch("prepare-base-link");
  let base = link_parent.join("runtime");
  std::os::unix::fs::symlink(&real, &base).expect("base symlink");

  let err = prepare_socket_dir(&base.join("app/com.discordapp.Discord"))
    .expect_err("symlinked base must fail");
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
  assert!(
    !real.join("app").exists(),
    "symlink target must stay untouched"
  );
  let _ = std::fs::remove_dir_all(&real);
  let _ = std::fs::remove_dir_all(&link_parent);
}

/// Preparing the preferred dir creates `app/` and the official app dir.
#[test]
fn prepare_socket_dir_creates_official_dirs() {
  let base = scratch("prepare");
  let dir = base.join("app/com.discordapp.Discord");
  prepare_socket_dir(&dir).expect("prepare preferred dir");
  assert!(base.join("app").is_dir());
  assert!(dir.is_dir());
  let _ = std::fs::remove_dir_all(&base);
}

/// Only the exact official layout is accepted, never arbitrary app dirs.
#[test]
fn prepare_socket_dir_rejects_wrong_layout() {
  let base = scratch("prepare-wrong-layout");
  let err = prepare_socket_dir(&base.join("app/not-discord")).expect_err("wrong layout must fail");
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
  assert!(!base.join("app").exists(), "nothing may be created");
  let _ = std::fs::remove_dir_all(&base);
}

/// Normalized variants of shared temp dirs are refused before creation.
#[test]
fn prepare_socket_dir_refuses_normalized_shared_temp() {
  for dir in [
    "/tmp/../tmp/app/com.discordapp.Discord",
    "//tmp/app/com.discordapp.Discord",
    "/var/tmp/../tmp/app/com.discordapp.Discord",
  ] {
    let err = prepare_socket_dir(Path::new(dir)).expect_err("shared temp must fail");
    assert_eq!(
      err.kind(),
      std::io::ErrorKind::InvalidInput,
      "unexpected error for {dir}: {err}"
    );
  }
}

/// A symlinked `app/` is refused instead of followed during creation.
#[test]
fn prepare_socket_dir_refuses_symlinked_app_dir() {
  let base = scratch("prepare-app-link");
  let outside = scratch("prepare-app-outside");
  std::os::unix::fs::symlink(&outside, base.join("app")).expect("app symlink");
  let err = prepare_socket_dir(&base.join("app/com.discordapp.Discord"))
    .expect_err("symlinked app must fail");
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
  assert!(!outside.join("com.discordapp.Discord").exists());
  let _ = std::fs::remove_dir_all(&base);
  let _ = std::fs::remove_dir_all(&outside);
}

/// A symlinked official app dir is refused instead of followed.
#[test]
fn prepare_socket_dir_refuses_symlinked_flatpak_dir() {
  let base = scratch("prepare-flatpak-link");
  let outside = scratch("prepare-flatpak-outside");
  std::fs::create_dir_all(base.join("app")).expect("app dir");
  std::os::unix::fs::symlink(&outside, base.join("app/com.discordapp.Discord"))
    .expect("flatpak symlink");
  let err = prepare_socket_dir(&base.join("app/com.discordapp.Discord"))
    .expect_err("symlinked app dir must fail");
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
  let _ = std::fs::remove_dir_all(&base);
  let _ = std::fs::remove_dir_all(&outside);
}

/// A private dir must belong to us and reject group/other writes.
#[test]
fn private_dir_requires_euid_and_no_group_other_write() {
  let dir = scratch("private-mode");
  validate_private_dir(&dir).expect("0700 scratch must pass");

  std::fs::set_permissions(&dir, Permissions::from_mode(0o770)).expect("chmod 0770");
  assert!(
    validate_private_dir(&dir).is_err(),
    "group-writable dir must fail"
  );

  std::fs::set_permissions(&dir, Permissions::from_mode(0o702)).expect("chmod 0702");
  assert!(
    validate_private_dir(&dir).is_err(),
    "other-writable dir must fail"
  );

  assert!(
    validate_private_dir(Path::new("/tmp")).is_err(),
    "shared temp root must fail the private policy"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// A group-writable ancestor is rejected even when the final dir is 0700.
#[test]
fn private_dir_rejects_group_writable_ancestor() {
  let base = scratch("private-ancestor-0770");
  let ancestor = base.join("shared");
  let dir = ancestor.join("private");
  std::fs::create_dir(&ancestor).expect("ancestor dir");
  std::fs::set_permissions(&ancestor, Permissions::from_mode(0o770)).expect("chmod ancestor");
  std::fs::DirBuilder::new()
    .mode(0o700)
    .create(&dir)
    .expect("private dir");

  let err = validate_private_dir(&dir).expect_err("group-writable ancestor must fail");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

  let _ = std::fs::remove_dir_all(&base);
}

/// A group-writable grandparent is rejected even when nearer components
/// are private; this exercises every component of the chain.
#[test]
fn private_dir_rejects_group_writable_grandparent() {
  let base = scratch("private-grandparent-0770");
  let outer = base.join("outer");
  let middle = outer.join("middle");
  let private = middle.join("private");
  std::fs::DirBuilder::new()
    .mode(0o700)
    .create(&outer)
    .expect("outer dir");
  std::fs::create_dir(&middle).expect("middle dir");
  std::fs::set_permissions(&middle, Permissions::from_mode(0o770)).expect("chmod middle");
  std::fs::DirBuilder::new()
    .mode(0o700)
    .create(&private)
    .expect("private dir");

  let err = validate_private_dir(&private).expect_err("group-writable grandparent must fail");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

  let _ = std::fs::remove_dir_all(&base);
}

/// An ancestor without group/other write is trusted, even at 0755.
#[test]
fn private_dir_accepts_read_only_ancestor() {
  let base = scratch("private-ancestor-0755");
  let ancestor = base.join("shared");
  let dir = ancestor.join("private");
  std::fs::create_dir(&ancestor).expect("ancestor dir");
  std::fs::set_permissions(&ancestor, Permissions::from_mode(0o755)).expect("chmod ancestor");
  std::fs::DirBuilder::new()
    .mode(0o700)
    .create(&dir)
    .expect("private dir");

  validate_private_dir(&dir).expect("0755 ancestor must pass");

  let _ = std::fs::remove_dir_all(&base);
}

/// Preparation refuses a group-writable ancestor before creating the child.
#[test]
fn prepare_private_dir_does_not_create_under_writable_ancestor() {
  let base = scratch("prepare-ancestor-0770");
  let ancestor = base.join("shared");
  std::fs::create_dir(&ancestor).expect("ancestor dir");
  std::fs::set_permissions(&ancestor, Permissions::from_mode(0o770)).expect("chmod ancestor");
  let child = ancestor.join("child");

  let err = prepare_private_dir(&child).expect_err("group-writable ancestor must fail");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  assert!(!child.exists(), "child must not be created");

  let _ = std::fs::remove_dir_all(&base);
}

/// Shared temp roots pass only under their sticky-bit policy; `/var/tmp`
/// passes when its chain is trusted, but a user-namespace sandbox may reject
/// `/var` itself, which is expected and documented.
#[test]
fn shared_temp_policy_accepts_tmp_and_rejects_non_sticky() {
  let tmp = Path::new("/tmp");
  if tmp.is_dir() {
    validate_shared_temp(tmp).expect("/tmp must pass the shared temp policy");
  }

  let dir = scratch("shared-temp");
  std::fs::set_permissions(&dir, Permissions::from_mode(0o777)).expect("chmod 0777");
  let err = validate_alias_dir(&dir).expect_err("non-sticky 0777 dir must fail");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

  let var_tmp = Path::new("/var/tmp");
  if var_tmp.is_dir() {
    match validate_shared_temp(var_tmp) {
      Ok(()) => {}
      Err(err) => {
        let ctx = NamespaceContext::atual();
        assert!(
          ctx.overflow_uid.is_some(),
          "/var/tmp failed outside the overflow sandbox: {err}"
        );
        let message = err.to_string();
        assert!(
          message.contains("/var") && message.contains("not a mount point"),
          "unexpected /var/tmp failure: {message}"
        );
      }
    }
  }
  let _ = std::fs::remove_dir_all(&dir);
}

/// Empty, relative, root, symlinked, and file paths are all refused.
#[test]
fn relative_empty_root_and_symlink_paths_are_rejected() {
  for path in ["", "relative", "/"] {
    let err = validate_private_dir(Path::new(path)).expect_err("shape must be refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "path {path:?}");
  }

  let dir = scratch("reject-shape");
  let link = dir.join("link");
  std::os::unix::fs::symlink(&dir, &link).expect("symlink");
  assert!(
    validate_private_dir(&link).is_err(),
    "symlinked final component must fail"
  );

  let file = dir.join("file");
  std::fs::write(&file, b"not a dir").expect("regular file");
  assert!(
    validate_private_dir(&file).is_err(),
    "regular file must fail"
  );
  let _ = std::fs::remove_dir_all(&dir);
}

/// A lexical `..` after a symlink is refused: the validated path must be
/// the path actually used.
#[test]
fn validate_private_dir_rejects_parent_dir_after_symlink() {
  let base = scratch("parent-dir");
  let outside = scratch("parent-dir-outside");
  let link = base.join("link");
  std::os::unix::fs::symlink(&outside, &link).expect("symlink");
  let tricky = link.join("..");

  let err = validate_private_dir(&tricky).expect_err("parent-dir component must fail");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

  let _ = std::fs::remove_dir_all(&base);
  let _ = std::fs::remove_dir_all(&outside);
}

/// Private dirs are created 0700; a symlinked parent is never followed.
#[test]
fn prepare_private_dir_creates_0700_and_refuses_symlinked_parent() {
  let base = scratch("prepare-private");
  let dir = base.join("new");
  prepare_private_dir(&dir).expect("create private dir");
  let meta = std::fs::symlink_metadata(&dir).expect("dir metadata");
  assert_eq!(meta.mode() & 0o777, 0o700);
  assert_eq!(meta.uid(), current_euid());

  let outside = scratch("prepare-private-outside");
  let link_parent = base.join("link-parent");
  std::os::unix::fs::symlink(&outside, &link_parent).expect("symlinked parent");
  let err =
    prepare_private_dir(&link_parent.join("child")).expect_err("symlinked parent must fail");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  assert!(
    !outside.join("child").exists(),
    "symlink target must stay untouched"
  );

  let _ = std::fs::remove_dir_all(&base);
  let _ = std::fs::remove_dir_all(&outside);
}

/// Bind dirs require exact 0700, while aliases may keep the 0755 private
/// policy; a 0600 dir is private but still not a valid bind directory.
#[test]
fn bind_dir_requires_exact_0700_but_private_dir_allows_0755() {
  let dir = scratch("bind-mode");
  let set = |mode| std::fs::set_permissions(&dir, Permissions::from_mode(mode));

  set(0o700).expect("chmod 0700");
  validate_private_dir(&dir).expect("private dir accepts 0700");
  validate_bind_dir(&dir).expect("bind dir accepts 0700");

  set(0o755).expect("chmod 0755");
  validate_private_dir(&dir).expect("private dir accepts 0755");
  let err = validate_bind_dir(&dir).expect_err("bind dir must reject 0755");
  assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  assert!(err.to_string().contains("0700"), "unexpected: {err}");

  set(0o600).expect("chmod 0600");
  validate_private_dir(&dir).expect("private dir accepts 0600");
  let err = validate_bind_dir(&dir).expect_err("bind dir must reject 0600");
  assert!(err.to_string().contains("0700"), "unexpected: {err}");

  set(0o700).expect("restore 0700");
  let _ = std::fs::remove_dir_all(&dir);
}
