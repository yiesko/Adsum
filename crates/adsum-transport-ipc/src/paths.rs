//! Unix socket placement: Flatpak-preferred bind, conventional bases, and
//! alias directories for every known client layout. Helpers are best-effort
//! and never touch foreign files.

#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::ffi::OsStr;
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Official Flatpak Discord app id, laid out as `app/<id>/discord-ipc-N`.
const FLATPAK_DISCORD_APP_ID: &str = "com.discordapp.Discord";

/// Per-base layouts that receive fan-out links, in publish order.
const ALIAS_LAYOUTS: [&str; 14] = [
  "",
  "app/com.discordapp.Discord",
  "app/com.discordapp.DiscordCanary",
  "app/com.discordapp.DiscordPTB",
  "app/dev.vencord.Vesktop",
  "app/org.equicord.equibop",
  ".flatpak/com.discordapp.Discord/xdg-run",
  ".flatpak/com.discordapp.DiscordCanary/xdg-run",
  ".flatpak/com.discordapp.DiscordPTB/xdg-run",
  ".flatpak/dev.vencord.Vesktop/xdg-run",
  ".flatpak/org.equicord.equibop/xdg-run",
  "snap.discord",
  "snap.discord-canary",
  "snap.discord-ptb",
];

/// Conventional IPC base dirs in official resolution order
/// (`XDG_RUNTIME_DIR` → `TMPDIR` → `TMP` → `TEMP` → `/tmp`). Existing bases
/// are canonicalized once so validation and use see the same real path.
pub fn socket_dir_candidates() -> Vec<PathBuf> {
  canonicalize_candidate_bases(candidate_dirs_from([
    ("XDG_RUNTIME_DIR", std::env::var("XDG_RUNTIME_DIR").ok()),
    ("TMPDIR", std::env::var("TMPDIR").ok()),
    ("TMP", std::env::var("TMP").ok()),
    ("TEMP", std::env::var("TEMP").ok()),
  ]))
}

/// Resolve existing absolute bases once, preserving order and deduplicating
/// again; relative or `..` paths stay raw for validation to reject.
fn canonicalize_candidate_bases(bases: Vec<PathBuf>) -> Vec<PathBuf> {
  let mut resolved = Vec::with_capacity(bases.len());
  for path in bases {
    let path = if path.is_absolute()
      && !path
        .components()
        .any(|component| component == Component::ParentDir)
    {
      std::fs::canonicalize(&path).unwrap_or(path)
    } else {
      path
    };
    if !resolved.contains(&path) {
      resolved.push(path);
    }
  }
  resolved
}

/// Pure core of [`socket_dir_candidates`]: first-seen order, blanks and
/// duplicates dropped, `/tmp` appended when no candidate covers it.
/// Separated for unit tests (environment mutation is process-global).
#[must_use]
pub fn candidate_dirs_from(vars: [(&str, Option<String>); 4]) -> Vec<PathBuf> {
  let mut dirs = Vec::new();
  for (_, value) in vars {
    if let Some(dir) = value {
      let dir = dir.trim_end_matches('/');
      if !dir.is_empty() {
        let path = PathBuf::from(dir);
        if !dirs.contains(&path) {
          dirs.push(path);
        }
      }
    }
  }
  let fallback = PathBuf::from("/tmp");
  if !dirs.contains(&fallback) {
    dirs.push(fallback);
  }
  dirs
}

/// Existing alias dirs below `bases`, in publish order; symlinked layouts
/// are skipped so fan-out cannot escape the runtime tree.
pub(crate) fn socket_alias_dirs(bases: &[PathBuf]) -> Vec<PathBuf> {
  let mut dirs = Vec::with_capacity(bases.len() * ALIAS_LAYOUTS.len());
  for base in bases {
    for layout in ALIAS_LAYOUTS {
      let Some(dir) = existing_alias_dir(base, layout) else {
        continue;
      };
      if dirs.contains(&dir) {
        continue;
      }
      if let Err(err) = validate_alias_dir(&dir) {
        tracing::debug!("[ipc] Skipping alias dir {}: {err}", dir.display());
        continue;
      }
      dirs.push(dir);
    }
  }
  dirs
}

/// Resolve one layout below `base`, requiring every component to be a real
/// directory (not a symlink).
fn existing_alias_dir(base: &Path, layout: &str) -> Option<PathBuf> {
  if !is_real_dir(base) {
    return None;
  }
  if layout.is_empty() {
    return Some(base.to_path_buf());
  }
  let mut path = base.to_path_buf();
  for component in Path::new(layout).components() {
    let Component::Normal(part) = component else {
      return None;
    };
    path.push(part);
    if !is_real_dir(&path) {
      return None;
    }
  }
  Some(path)
}

/// Whether `path` is a real directory; symlinks are not followed.
fn is_real_dir(path: &Path) -> bool {
  std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

/// Official Flatpak Discord socket dir from an optional `XDG_RUNTIME_DIR`
/// value; `None` for relative or shared-temp bases.
pub(crate) fn preferred_socket_dir_from(xdg: Option<&str>) -> Option<PathBuf> {
  let base = xdg?.trim_end_matches('/');
  if base.is_empty() {
    return None;
  }
  let base = normalize_absolute(Path::new(base))?;
  if base == Path::new("/") || base == Path::new("/tmp") || base == Path::new("/var/tmp") {
    return None;
  }
  Some(base.join("app").join(FLATPAK_DISCORD_APP_ID))
}

/// Lexically normalize `.`/`..` and repeated separators in an absolute path.
fn normalize_absolute(path: &Path) -> Option<PathBuf> {
  if !path.is_absolute() {
    return None;
  }
  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      Component::RootDir => normalized.push(Path::new("/")),
      Component::CurDir => {}
      Component::ParentDir => {
        normalized.pop();
      }
      Component::Normal(part) => normalized.push(part),
      Component::Prefix(_) => return None,
    }
  }
  if normalized.as_os_str().is_empty() {
    normalized.push(Path::new("/"));
  }
  Some(normalized)
}

/// Effective user id of the current process.
pub(crate) fn current_euid() -> u32 {
  // SAFETY: geteuid has no preconditions and always returns the caller's
  // effective uid; the call cannot fail.
  unsafe { libc::geteuid() }
}

/// Directory-chain trust inputs that depend on the caller's user namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NamespaceContext {
  /// Overflow UID accepted on anchored components, when the exception is on.
  overflow_uid: Option<u32>,
  /// UID read from `/proc/sys/kernel/overflowuid`, kept for error messages.
  raw_overflow_uid: Option<u32>,
  /// Why the exception is off; empty while it is on.
  inactive_reason: &'static str,
}

impl NamespaceContext {
  /// Process-wide context, probed once from `/proc` and a Linux `statx`.
  pub(crate) fn atual() -> &'static Self {
    static CONTEXT: OnceLock<NamespaceContext> = OnceLock::new();
    CONTEXT.get_or_init(Self::sondar)
  }

  /// Probe the real namespace; any read, parse, or `statx` failure turns the
  /// overflow exception off (fail closed).
  fn sondar() -> Self {
    let Ok(overflow) = std::fs::read_to_string("/proc/sys/kernel/overflowuid") else {
      return Self::inactive("cannot read /proc/sys/kernel/overflowuid", None);
    };
    let raw_overflow_uid = overflow.trim().parse::<u32>().ok();
    let Ok(uid_map) = std::fs::read_to_string("/proc/self/uid_map") else {
      return Self::inactive("cannot read /proc/self/uid_map", raw_overflow_uid);
    };
    let mnt_supported = facts_of(Path::new("/")).is_ok_and(|facts| facts.mnt_id.is_some());
    Self::evaluate(&uid_map, &overflow, mnt_supported)
  }

  /// Pure evaluation of `uid_map`, `overflowuid`, and mount-id support; the
  /// exception is active only when root is unmapped and the UID is not mapped.
  fn evaluate(uid_map: &str, overflow: &str, mnt_supported: bool) -> Self {
    let Some(raw_overflow_uid) = overflow.trim().parse::<u32>().ok() else {
      return Self::inactive("invalid /proc/sys/kernel/overflowuid", None);
    };
    let Some(map) = UidMap::parse(uid_map) else {
      return Self::inactive("invalid /proc/self/uid_map", Some(raw_overflow_uid));
    };
    if map.parent_uid_mapped(0) {
      return Self::inactive("parent uid 0 is mapped", Some(raw_overflow_uid));
    }
    if map.inside_uid_mapped(raw_overflow_uid) {
      return Self::inactive(
        "overflow uid is mapped inside the namespace",
        Some(raw_overflow_uid),
      );
    }
    if !mnt_supported {
      return Self::inactive("statx mount ids are unavailable", Some(raw_overflow_uid));
    }
    Self {
      overflow_uid: Some(raw_overflow_uid),
      raw_overflow_uid: Some(raw_overflow_uid),
      inactive_reason: "",
    }
  }

  /// Context with the overflow exception disabled for `reason`.
  fn inactive(reason: &'static str, raw_overflow_uid: Option<u32>) -> Self {
    Self {
      overflow_uid: None,
      raw_overflow_uid,
      inactive_reason: reason,
    }
  }
}

/// Parsed `/proc/self/uid_map`: `(inside, outside, count)` ranges.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UidMap {
  ranges: Vec<UidRange>,
}

/// One `uid_map` line; `count` is non-zero by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UidRange {
  inside: u32,
  outside: u32,
  count: u32,
}

impl UidMap {
  /// Parse every line; malformed lines, extra fields, and zero counts are
  /// refused so a partial namespace view can never enable the exception.
  fn parse(input: &str) -> Option<Self> {
    let mut ranges = Vec::new();
    for line in input.lines() {
      let mut fields = line.split_whitespace();
      let inside = fields.next()?.parse::<u32>().ok()?;
      let outside = fields.next()?.parse::<u32>().ok()?;
      let count = fields.next()?.parse::<u32>().ok()?;
      if fields.next().is_some() || count == 0 {
        return None;
      }
      ranges.push(UidRange {
        inside,
        outside,
        count,
      });
    }
    if ranges.is_empty() {
      return None;
    }
    Some(Self { ranges })
  }

  /// Whether a parent-namespace UID falls in an `outside` range.
  fn parent_uid_mapped(&self, uid: u32) -> bool {
    self.ranges.iter().any(|range| range.outside_contains(uid))
  }

  /// Whether a namespace UID falls in an `inside` range.
  fn inside_uid_mapped(&self, uid: u32) -> bool {
    self.ranges.iter().any(|range| range.inside_contains(uid))
  }
}

impl UidRange {
  /// Whether `uid` is inside the `outside` interval (u64 avoids overflow).
  fn outside_contains(&self, uid: u32) -> bool {
    Self::contains(self.outside, self.count, uid)
  }

  /// Whether `uid` is inside the `inside` interval (u64 avoids overflow).
  fn inside_contains(&self, uid: u32) -> bool {
    Self::contains(self.inside, self.count, uid)
  }

  /// Half-open range check with a 64-bit end.
  fn contains(start: u32, count: u32, uid: u32) -> bool {
    let start = u64::from(start);
    let end = start + u64::from(count);
    let uid = u64::from(uid);
    uid >= start && uid < end
  }
}

/// Owner, mode, and mount identity of one directory-chain component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Facts {
  uid: u32,
  mode: u32,
  mnt_id: Option<u64>,
}

/// `S_IFMT` mask in `Facts::mode` (portable literal, not `libc::mode_t`).
const MODE_TYPE_MASK: u32 = 0o170000;
/// Directory type in `Facts::mode`.
const MODE_DIRECTORY: u32 = 0o040000;

/// Metadata for `path` without following symlinks; Linux adds the mount id.
fn facts_of(path: &Path) -> io::Result<Facts> {
  #[cfg(target_os = "linux")]
  if let Some(facts) = statx_facts(path)? {
    return Ok(facts);
  }
  let meta = std::fs::symlink_metadata(path)?;
  Ok(Facts {
    uid: meta.uid(),
    mode: meta.mode(),
    mnt_id: None,
  })
}

/// Linux `statx` metadata; `Ok(None)` means the syscall failed and the caller
/// may fall back, while a missing UID/MODE mask is a hard failure.
#[cfg(target_os = "linux")]
fn statx_facts(path: &Path) -> io::Result<Option<Facts>> {
  let c_path = CString::new(path.as_os_str().as_bytes())
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains nul"))?;
  let mut statx = std::mem::MaybeUninit::<libc::statx>::uninit();
  // SAFETY: `c_path` is NUL-terminated and lives for the call; `AT_FDCWD` and
  // the flags are valid; `statx` points to writable storage for one struct.
  let result = unsafe {
    libc::statx(
      libc::AT_FDCWD,
      c_path.as_ptr(),
      libc::AT_SYMLINK_NOFOLLOW,
      libc::STATX_BASIC_STATS | libc::STATX_MNT_ID,
      statx.as_mut_ptr(),
    )
  };
  if result != 0 {
    return Ok(None);
  }
  // SAFETY: `statx` returned 0, so the kernel initialized the whole struct.
  let statx = unsafe { statx.assume_init() };
  let required = libc::STATX_UID | libc::STATX_MODE;
  if statx.stx_mask & required != required {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "statx did not report uid and mode",
    ));
  }
  let mnt_id = (statx.stx_mask & libc::STATX_MNT_ID != 0).then_some(statx.stx_mnt_id);
  Ok(Some(Facts {
    uid: statx.stx_uid,
    mode: u32::from(statx.stx_mode),
    mnt_id,
  }))
}

/// Directory policy applied before a path may host a socket or alias link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirPolicy {
  /// User-private dir: owned by us, not writable by group or others.
  Private,
  /// Shared sticky temp root (`/tmp`, `/var/tmp`).
  SharedTemp,
}

/// Validate `path` against `policy`.
pub(crate) fn validate_dir(path: &Path, policy: DirPolicy) -> io::Result<()> {
  match policy {
    DirPolicy::Private => validate_private_dir(path),
    DirPolicy::SharedTemp => validate_shared_temp(path),
  }
}

/// Require a user-private directory: trusted real dir chain, owned by the
/// effective uid, and not writable by group or others.
pub(crate) fn validate_private_dir(path: &Path) -> io::Result<()> {
  let facts = validate_private_dir_facts(path)?;
  if facts.mode & 0o022 != 0 {
    return Err(invalid_path(path, "writable by group or others"));
  }
  Ok(())
}

/// Private-directory core; returns the final facts so bind dirs can add the
/// stricter `0700` requirement without a second metadata read.
fn validate_private_dir_facts(path: &Path) -> io::Result<Facts> {
  let normalized = normalize_dir(path)?;
  let facts = validate_real_dir_chain(&normalized, NamespaceContext::atual(), false)?;
  let euid = current_euid();
  if facts.uid != euid {
    return Err(invalid_owner_path(&normalized, facts.uid));
  }
  Ok(facts)
}

/// Require a bind directory: private policy plus exact `0700` permissions on
/// the final directory.
pub(crate) fn validate_bind_dir(path: &Path) -> io::Result<()> {
  let facts = validate_private_dir_facts(path)?;
  if facts.mode & 0o777 != 0o700 {
    return Err(invalid_path(path, "bind directory must be 0700"));
  }
  Ok(())
}

/// Require a shared temp root: canonical `/tmp` or `/var/tmp`, sticky, owned
/// by root, the effective uid, or the anchored overflow UID, and writable.
pub(crate) fn validate_shared_temp(path: &Path) -> io::Result<()> {
  validate_shared_temp_canonical(path).map(|_| ())
}

/// Validate a shared temp root and return its canonical path (resolving
/// platform symlinks such as macOS `/tmp`).
///
/// The whole canonical chain is checked, so an unmapped `/var` refuses
/// `/var/tmp` in a sandbox; `/tmp` remains the practical fallback.
fn validate_shared_temp_canonical(path: &Path) -> io::Result<PathBuf> {
  let normalized = normalize_dir(path)?;
  let canonical = std::fs::canonicalize(&normalized)?;
  if !is_canonical_shared_temp(&canonical) {
    return Err(invalid_path(&normalized, "not a shared temp root"));
  }
  validate_real_dir_chain(&canonical, NamespaceContext::atual(), true)?;
  probe_temp_write(&canonical)?;
  Ok(canonical)
}

/// Validate a fan-out alias dir: shared temp roots keep the temp policy,
/// anything else must be user-private.
pub(crate) fn validate_alias_dir(path: &Path) -> io::Result<()> {
  let policy = if is_shared_temp_root(path) {
    DirPolicy::SharedTemp
  } else {
    DirPolicy::Private
  };
  validate_dir(path, policy)
}

/// Create a private dir with mode `0700`; an existing dir is revalidated as
/// private. Every parent must already exist as a trusted real directory.
pub(crate) fn prepare_private_dir(path: &Path) -> io::Result<()> {
  let normalized = normalize_dir(path)?;
  let parent = normalized
    .parent()
    .ok_or_else(|| invalid_path(&normalized, "missing parent directory"))?;
  validate_real_dir_chain(parent, NamespaceContext::atual(), true)?;
  match std::fs::DirBuilder::new().mode(0o700).create(&normalized) {
    Ok(()) => {}
    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
    Err(err) => return Err(err),
  }
  validate_private_dir(&normalized)
}

/// Name of the per-uid fallback socket dir under a shared temp root.
#[must_use]
pub(crate) fn fallback_dir_name(euid: u32) -> String {
  format!(".adsum-ipc-{euid}")
}

/// Prepare the per-uid fallback dir under the first usable shared temp root;
/// an existing dir that fails validation is left untouched.
pub(crate) fn prepare_fallback_dir(roots: &[PathBuf], euid: u32) -> Option<PathBuf> {
  for root in roots {
    let canonical = match validate_shared_temp_canonical(root) {
      Ok(canonical) => canonical,
      Err(err) => {
        tracing::debug!("[ipc] Skipping fallback root {}: {err}", root.display());
        continue;
      }
    };
    let dir = canonical.join(fallback_dir_name(euid));
    match prepare_private_dir(&dir) {
      Ok(()) => return Some(dir),
      Err(err) => {
        tracing::debug!("[ipc] Fallback dir {} unusable: {err}", dir.display());
      }
    }
  }
  None
}

/// Build the bind order: preferred dir, conventional bases, then fallback;
/// shared temp roots are filtered and duplicates dropped in place.
#[must_use]
pub(crate) fn bind_dirs_in_order(
  bases: Vec<PathBuf>,
  preferred: Option<&Path>,
  fallback: Option<PathBuf>,
) -> Vec<PathBuf> {
  let mut dirs = Vec::with_capacity(bases.len() + 2);
  let candidates = preferred
    .map(Path::to_path_buf)
    .into_iter()
    .chain(bases)
    .chain(fallback);
  for dir in candidates {
    if is_shared_temp_root(&dir) || dirs.contains(&dir) {
      continue;
    }
    dirs.push(dir);
  }
  dirs
}

/// Normalize an absolute non-root path, refusing empty/relative/root input
/// and lexical `..` (the validated path must be the path actually used).
fn normalize_dir(path: &Path) -> io::Result<PathBuf> {
  if path
    .components()
    .any(|component| component == Component::ParentDir)
  {
    return Err(invalid_path(path, "contains a parent-directory component"));
  }
  normalize_absolute(path)
    .filter(|normalized| normalized != Path::new("/"))
    .ok_or_else(|| invalid_path(path, "not an absolute non-root directory"))
}

/// Require every component of `path` to be a real, trusted directory and
/// return the final facts. Only ancestors may use the overflow exception,
/// unless `anchor_final` marks a canonical temp root that is itself anchored.
fn validate_real_dir_chain(
  path: &Path,
  ctx: &NamespaceContext,
  anchor_final: bool,
) -> io::Result<Facts> {
  let euid = current_euid();
  let mut components = path.components().peekable();
  let mut current = PathBuf::new();
  let mut previous_mnt_id = None;
  let mut last = None;
  while let Some(component) = components.next() {
    let is_final = components.peek().is_none();
    current.push(component.as_os_str());
    let facts = facts_of(&current)?;
    if facts.mode & MODE_TYPE_MASK != MODE_DIRECTORY {
      return Err(invalid_path(&current, "not a real directory"));
    }
    let is_mount = matches!(component, Component::RootDir)
      || matches!(
        (previous_mnt_id, facts.mnt_id),
        (Some(previous), Some(current)) if current != previous
      );
    let allow_overflow = !is_final || anchor_final;
    validate_chain_component(&current, &facts, euid, is_mount, allow_overflow, ctx)?;
    previous_mnt_id = facts.mnt_id;
    last = Some(facts);
  }
  last.ok_or_else(|| invalid_path(path, "empty directory path"))
}

/// Check one chain component: owner trust, sticky shared temp roots, and no
/// group/other write. `allow_overflow` is false for the final bind directory.
fn validate_chain_component(
  path: &Path,
  facts: &Facts,
  euid: u32,
  is_mount: bool,
  allow_overflow: bool,
  ctx: &NamespaceContext,
) -> io::Result<()> {
  let uid = facts.uid;
  let shared_temp = is_shared_temp_root(path);
  let trusted = uid == 0
    || uid == euid
    || (allow_overflow && ctx.overflow_uid == Some(uid) && (is_mount || shared_temp));
  if !trusted {
    return Err(owner_error(path, uid, allow_overflow, ctx));
  }
  if shared_temp {
    if facts.mode & 0o1000 == 0 {
      return Err(invalid_path(path, "missing the sticky bit"));
    }
  } else if facts.mode & 0o022 != 0 {
    return Err(invalid_path(path, "writable by group or others"));
  }
  Ok(())
}

/// Refusal for an untrusted component, naming the path, UID, and why.
fn owner_error(path: &Path, uid: u32, allow_overflow: bool, ctx: &NamespaceContext) -> io::Error {
  if ctx.overflow_uid.is_none() && ctx.raw_overflow_uid == Some(uid) {
    return io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "overflow uid, but exception inactive: {}: {} (uid {uid})",
        ctx.inactive_reason,
        path.display()
      ),
    );
  }
  let reason = if !allow_overflow && ctx.overflow_uid == Some(uid) {
    "overflow uid, but the final directory cannot use the exception"
  } else if allow_overflow && ctx.overflow_uid == Some(uid) {
    "overflow uid, but not a mount point or shared temp root"
  } else if allow_overflow {
    "not owned by root or the effective uid"
  } else {
    "not owned by the effective uid"
  };
  io::Error::new(
    io::ErrorKind::InvalidInput,
    format!("{reason}: {} (uid {uid})", path.display()),
  )
}

/// Refusal for a final directory not owned by the effective UID, with the
/// observed UID for diagnostics.
fn invalid_owner_path(path: &Path, uid: u32) -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    format!(
      "not owned by the effective uid: {} (uid {uid})",
      path.display()
    ),
  )
}

/// Prove effective-uid write access by creating and removing an exclusive
/// file inside `dir` (`O_EXCL` never follows a final symlink).
fn probe_temp_write(dir: &Path) -> io::Result<()> {
  static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);
  probe_temp_write_with(dir, &PROBE_SEQ)
}

/// Try up to eight exclusive probe names, retrying only collisions; other
/// errors propagate.
fn probe_temp_write_with(dir: &Path, seq: &AtomicU64) -> io::Result<()> {
  for _ in 0..8 {
    let value = seq.fetch_add(1, Ordering::Relaxed);
    let probe = dir.join(format!(".adsum-ipc-probe-{}-{value}", std::process::id()));
    let file = match std::fs::OpenOptions::new()
      .write(true)
      .create_new(true)
      .mode(0o600)
      .open(&probe)
    {
      Ok(file) => file,
      Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
      Err(err) => return Err(err),
    };
    drop(file);
    return std::fs::remove_file(&probe);
  }
  Err(io::Error::new(
    io::ErrorKind::AlreadyExists,
    "temporary IPC probe name exhausted",
  ))
}

/// Whether `path` is or resolves to `/tmp` or `/var/tmp`.
fn is_shared_temp_root(path: &Path) -> bool {
  if path == Path::new("/tmp") || path == Path::new("/var/tmp") {
    return true;
  }
  std::fs::canonicalize(path).is_ok_and(|canonical| is_canonical_shared_temp(&canonical))
}

/// Whether canonical `path` is `/tmp` or `/var/tmp`, resolving platform
/// symlinks (macOS maps both into `/private`).
fn is_canonical_shared_temp(path: &Path) -> bool {
  ["/tmp", "/var/tmp"]
    .iter()
    .any(|shared| std::fs::canonicalize(shared).is_ok_and(|resolved| resolved == path))
}

/// Create the official Flatpak socket dir and its `app/` parent without
/// following symlinks; `dir` must be `<runtime>/app/<official-id>`.
pub(crate) fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
  if dir.file_name() != Some(OsStr::new(FLATPAK_DISCORD_APP_ID)) {
    return Err(invalid_path(dir, "not the official Flatpak dir"));
  }
  let app = dir
    .parent()
    .filter(|path| path.file_name() == Some(OsStr::new("app")))
    .ok_or_else(|| invalid_path(dir, "missing app parent"))?;
  let base = app
    .parent()
    .ok_or_else(|| invalid_path(app, "missing runtime base"))?;
  validate_private_dir(base)?;
  let canonical = std::fs::canonicalize(base)?;
  if is_canonical_shared_temp(&canonical) {
    return Err(invalid_path(base, "shared temp base"));
  }
  prepare_private_dir(app)?;
  prepare_private_dir(dir)
}

/// Build the refusal error for a path that cannot be used as a socket dir.
fn invalid_path(path: &Path, reason: &str) -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    format!("{reason}: {}", path.display()),
  )
}

#[cfg(test)]
#[path = "../tests/unit/paths/mod.rs"]
mod tests;
