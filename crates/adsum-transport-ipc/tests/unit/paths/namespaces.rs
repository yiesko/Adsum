use super::*;

/// Sandbox and host maps parse; malformed, extra-field, and zero ranges fail.
#[test]
fn uid_map_parses_sandbox_host_and_rejects_invalid() {
  let sandbox = UidMap::parse("         1000       1000          1\n").expect("sandbox map");
  assert!(!sandbox.parent_uid_mapped(0));
  assert!(sandbox.parent_uid_mapped(1000));
  assert!(sandbox.inside_uid_mapped(1000));
  assert!(!sandbox.inside_uid_mapped(65534));

  let host = UidMap::parse("0 0 4294967295\n").expect("host map");
  assert!(host.parent_uid_mapped(0));
  assert!(host.parent_uid_mapped(4294967294));
  assert!(host.inside_uid_mapped(0));

  for invalid in [
    "",
    "1000 1000\n",
    "1000 1000 1 extra\n",
    "a b c\n",
    "1000 1000 0\n",
    "1000 1000 1\nbogus\n",
  ] {
    assert!(UidMap::parse(invalid).is_none(), "must reject {invalid:?}");
  }

  let mapped = UidMap::parse("1000 1000 1\n65534 524288 1\n").expect("mapped overflow");
  assert!(mapped.inside_uid_mapped(65534));
}

/// Range ends are computed in u64, so a high start plus a full count is safe.
#[test]
fn uid_map_range_arithmetic_does_not_overflow() {
  let map = UidMap::parse("0 1 4294967295\n").expect("large range");
  assert!(!map.parent_uid_mapped(0));
  assert!(map.parent_uid_mapped(1));
  assert!(map.parent_uid_mapped(4294967295));
  assert!(map.inside_uid_mapped(4294967294));
}

/// The exception is active only when root is unmapped and mount ids exist.
#[test]
fn namespace_context_is_active_only_for_unmapped_root() {
  let active = NamespaceContext::evaluate("1000 1000 1\n", "65534\n", true);
  assert_eq!(active.overflow_uid, Some(65534));
  assert_eq!(active.raw_overflow_uid, Some(65534));
  assert!(active.inactive_reason.is_empty());

  let host = NamespaceContext::evaluate("0 0 4294967295\n", "65534\n", true);
  assert_eq!(host.overflow_uid, None);
  assert_eq!(host.raw_overflow_uid, Some(65534));
  assert!(host.inactive_reason.contains("parent uid 0"));

  let mapped = NamespaceContext::evaluate("1000 1000 1\n65534 524288 1\n", "65534\n", true);
  assert_eq!(mapped.overflow_uid, None);
  assert!(mapped.inactive_reason.contains("mapped inside"));
}

/// Invalid maps, invalid overflow values, and missing mount ids fail closed.
#[test]
fn namespace_context_turns_off_on_invalid_input_or_missing_mount_ids() {
  for (uid_map, overflow) in [
    ("bogus", "65534"),
    ("1000 1000 1", "not-a-uid"),
    ("1000 1000 0", "65534"),
  ] {
    let ctx = NamespaceContext::evaluate(uid_map, overflow, true);
    assert_eq!(
      ctx.overflow_uid, None,
      "map {uid_map:?} overflow {overflow:?}"
    );
    assert!(!ctx.inactive_reason.is_empty(), "map {uid_map:?}");
  }

  let no_mount = NamespaceContext::evaluate("1000 1000 1", "65534", false);
  assert_eq!(no_mount.overflow_uid, None);
  assert!(no_mount.inactive_reason.contains("mount ids"));
}

/// An active overflow UID passes only on a mount point or shared temp root.
#[test]
fn chain_component_allows_overflow_only_with_an_anchor() {
  let ctx = NamespaceContext::evaluate("1000 1000 1", "65534", true);
  let overflow = facts(65534, 0o040755, Some(2));
  validate_chain_component(Path::new("/mnt"), &overflow, 1000, true, true, &ctx)
    .expect("mount point anchor must pass");

  let temp = facts(65534, 0o041777, Some(2));
  validate_chain_component(Path::new("/tmp"), &temp, 1000, false, true, &ctx)
    .expect("shared temp anchor must pass");

  let err = validate_chain_component(Path::new("/usr"), &overflow, 1000, false, true, &ctx)
    .expect_err("unanchored overflow uid must fail");
  let message = err.to_string();
  assert!(message.contains("/usr"), "path missing: {message}");
  assert!(message.contains("65534"), "uid missing: {message}");
  assert!(
    message.contains("not a mount point"),
    "reason missing: {message}"
  );

  let err = validate_chain_component(Path::new("/final"), &overflow, 1000, false, false, &ctx)
    .expect_err("the final directory must never use the exception");
  let message = err.to_string();
  assert!(
    message.contains("final directory"),
    "final-directory reason missing: {message}"
  );
  assert!(message.contains("65534"), "uid missing: {message}");
}

/// An inactive exception rejects the overflow UID even on a mount point.
#[test]
fn chain_component_reports_inactive_exception() {
  let ctx = NamespaceContext::evaluate("0 0 4294967295", "65534", true);
  let overflow = facts(65534, 0o040755, Some(2));
  let err = validate_chain_component(Path::new("/"), &overflow, 1000, true, true, &ctx)
    .expect_err("inactive exception must reject even a mount point");
  let message = err.to_string();
  assert!(
    message.contains("exception inactive"),
    "reason missing: {message}"
  );
  assert!(message.contains("65534"), "uid missing: {message}");
  assert!(message.contains("parent uid 0"), "cause missing: {message}");
}

/// Under the systemd user-unit sandbox, host root is unmapped and the
/// anchored overflow UID must keep the runtime dir and `/tmp` usable.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the systemd user-unit sandbox; run via systemd-run --user"]
fn user_namespace_overflow_accepts_anchored_dirs() {
  let uid_map = std::fs::read_to_string("/proc/self/uid_map").expect("uid_map");
  let map = UidMap::parse(&uid_map).expect("parse uid_map");
  assert!(
    !map.parent_uid_mapped(0),
    "host root must be unmapped: {uid_map}"
  );
  let ctx = NamespaceContext::atual();
  assert!(
    ctx.overflow_uid.is_some(),
    "overflow exception must be active: {}",
    ctx.inactive_reason
  );

  let euid = current_euid();
  let runtime = std::env::var_os("XDG_RUNTIME_DIR")
    .map_or_else(|| PathBuf::from(format!("/run/user/{euid}")), PathBuf::from);
  validate_private_dir(&runtime).expect("runtime dir must pass the private policy");
  validate_shared_temp(Path::new("/tmp")).expect("/tmp must pass the shared temp policy");
}

/// An unanchored overflow-owned ancestor must be refused with the anchor
/// reason; separate mounts are skipped, and the test reports if none fits.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the systemd user-unit sandbox; run via systemd-run --user"]
fn user_namespace_overflow_rejects_unanchored_dirs() {
  let ctx = NamespaceContext::atual();
  let Some(overflow) = ctx.overflow_uid else {
    panic!("overflow exception must be active: {}", ctx.inactive_reason);
  };
  assert!(
    facts_of(Path::new("/"))
      .expect("root facts")
      .mnt_id
      .is_some(),
    "statx mount ids are required for this test"
  );

  // Prove the shared-temp validator checks the whole canonical chain: in
  // this sandbox `/var` is unanchored, so `/var/tmp` must be refused.
  if Path::new("/var/tmp").is_dir()
    && std::fs::canonicalize("/var/tmp").is_ok_and(|path| path == Path::new("/var/tmp"))
    && let (Ok(root), Ok(var)) = (facts_of(Path::new("/")), facts_of(Path::new("/var")))
    && var.uid == overflow
    && var.mode & MODE_TYPE_MASK == MODE_DIRECTORY
    && matches!((root.mnt_id, var.mnt_id), (Some(root), Some(var)) if root == var)
    && !is_shared_temp_root(Path::new("/var"))
  {
    let err = validate_shared_temp(Path::new("/var/tmp"))
      .expect_err("unanchored /var must refuse /var/tmp");
    let message = err.to_string();
    assert!(message.contains("/var"), "component missing: {message}");
    assert!(
      message.contains("not a mount point"),
      "anchor reason missing: {message}"
    );
  }

  // The final directory never uses the exception, so the anchor rule is
  // exercised through an ancestor: validate a child of the unanchored dir.
  let found = ["/usr", "/var", "/etc", "/opt"]
    .into_iter()
    .map(Path::new)
    .find_map(|ancestor| {
      let facts = facts_of(ancestor).ok()?;
      if facts.uid != overflow || facts.mode & MODE_TYPE_MASK != MODE_DIRECTORY {
        return None;
      }
      let parent_facts = facts_of(ancestor.parent()?).ok()?;
      let (Some(parent_mnt), Some(ancestor_mnt)) = (parent_facts.mnt_id, facts.mnt_id) else {
        return None;
      };
      if ancestor_mnt != parent_mnt || is_shared_temp_root(ancestor) {
        return None;
      }
      let child = std::fs::read_dir(ancestor)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))?
        .path();
      Some((ancestor.to_path_buf(), child))
    });
  let Some((ancestor, candidate)) = found else {
    eprintln!("skipping: no unmounted overflow-owned directory with a child dir available");
    return;
  };

  let err = validate_private_dir(&candidate).expect_err("unanchored overflow owner must fail");
  let message = err.to_string();
  assert!(
    message.contains(&ancestor.display().to_string()),
    "component missing: {message}"
  );
  assert!(
    message.contains(&overflow.to_string()),
    "uid missing: {message}"
  );
  assert!(
    message.contains("not a mount point"),
    "anchor reason missing: {message}"
  );
}
