//! Unix socket placement: Flatpak-preferred bind, conventional bases, and
//! fan-out symlinks for every known client layout. Helpers are best-effort
//! and never touch foreign files.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
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
/// (`XDG_RUNTIME_DIR` → `TMPDIR` → `TMP` → `TEMP` → `/tmp`).
pub fn socket_dir_candidates() -> Vec<PathBuf> {
  candidate_dirs_from([
    ("XDG_RUNTIME_DIR", std::env::var("XDG_RUNTIME_DIR").ok()),
    ("TMPDIR", std::env::var("TMPDIR").ok()),
    ("TMP", std::env::var("TMP").ok()),
    ("TEMP", std::env::var("TEMP").ok()),
  ])
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

/// Require a user-private directory: real dir chain, owned by the effective
/// uid, and not writable by group or others.
pub(crate) fn validate_private_dir(path: &Path) -> io::Result<()> {
  let normalized = normalize_dir(path)?;
  let meta = validate_real_dir_chain(&normalized)?;
  let euid = current_euid();
  if meta.uid() != euid {
    return Err(invalid_path(&normalized, "not owned by the effective uid"));
  }
  if meta.mode() & 0o022 != 0 {
    return Err(invalid_path(&normalized, "writable by group or others"));
  }
  Ok(())
}

/// Require a shared temp root: canonical `/tmp` or `/var/tmp`, sticky,
/// root- or self-owned, and writable by this effective uid.
pub(crate) fn validate_shared_temp(path: &Path) -> io::Result<()> {
  validate_shared_temp_canonical(path).map(|_| ())
}

/// Validate a shared temp root and return its canonical path (resolving
/// platform symlinks such as macOS `/tmp`).
fn validate_shared_temp_canonical(path: &Path) -> io::Result<PathBuf> {
  let normalized = normalize_dir(path)?;
  let canonical = std::fs::canonicalize(&normalized)?;
  if !is_canonical_shared_temp(&canonical) {
    return Err(invalid_path(&normalized, "not a shared temp root"));
  }
  let meta = std::fs::symlink_metadata(&canonical)?;
  if !meta.is_dir() {
    return Err(invalid_path(&normalized, "not a directory"));
  }
  let euid = current_euid();
  if meta.uid() != 0 && meta.uid() != euid {
    return Err(invalid_path(
      &normalized,
      "not owned by root or the effective uid",
    ));
  }
  if meta.mode() & 0o1000 == 0 {
    return Err(invalid_path(&normalized, "missing the sticky bit"));
  }
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

/// Create a `0700` private dir, revalidating an existing one; every parent
/// must already exist as a real directory.
pub(crate) fn prepare_private_dir(path: &Path) -> io::Result<()> {
  let normalized = normalize_dir(path)?;
  let parent = normalized
    .parent()
    .ok_or_else(|| invalid_path(&normalized, "missing parent directory"))?;
  validate_real_dir_chain(parent)?;
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

/// Require every component of `path` to be a real directory (never a
/// symlink) and return the final directory's metadata.
fn validate_real_dir_chain(path: &Path) -> io::Result<std::fs::Metadata> {
  let mut current = PathBuf::new();
  let mut last = None;
  for component in path.components() {
    current.push(component.as_os_str());
    let meta = std::fs::symlink_metadata(&current)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
      return Err(invalid_path(&current, "not a real directory"));
    }
    last = Some(meta);
  }
  last.ok_or_else(|| invalid_path(path, "empty directory path"))
}

/// Prove effective-uid write access by creating and removing an exclusive
/// file inside `dir` (`O_EXCL` never follows a final symlink).
fn probe_temp_write(dir: &Path) -> io::Result<()> {
  static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);
  let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
  let probe = dir.join(format!(".adsum-ipc-probe-{}-{seq}", std::process::id()));
  let file = std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .mode(0o600)
    .open(&probe)?;
  drop(file);
  std::fs::remove_file(&probe)
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

/// Whether `link` may be replaced by our live socket: it already points
/// at it, or it is a stale ours-shaped symlink (`discord-ipc-*`) whose
/// target is gone (previous crashed run). A symlink owned by a live
/// foreign service is never ours — stealing it would redirect that
/// service's clients to us.
fn link_reclaimable(link: &Path, bound: &Path) -> bool {
  let target = match std::fs::read_link(link) {
    Ok(target) => target,
    Err(_) => return false, // not a symlink: never touch.
  };
  // Resolve relative targets against the link's parent for the checks.
  let resolved = if target.is_absolute() {
    Cow::Borrowed(target.as_path())
  } else {
    match link.parent() {
      Some(parent) => Cow::Owned(parent.join(&target)),
      None => Cow::Borrowed(target.as_path()),
    }
  };
  if resolved.as_ref() == bound {
    return true;
  }
  let ours_shaped = target
    .file_name()
    .and_then(|name| name.to_str())
    .is_some_and(|name| name.starts_with("discord-ipc-"));
  ours_shaped && std::fs::symlink_metadata(resolved.as_ref()).is_err()
}

/// Symlink the bound `discord-ipc-{index}` socket into every other
/// candidate dir. Stale ours-shaped symlinks (`discord-ipc-*`) are
/// replaced; anything else (live sockets, foreign files, live foreign
/// links) is left alone; missing, symlinked, or unwritable dirs are skipped.
pub fn fanout_socket_link(dirs: &[PathBuf], bound_path: &str, file_name: &str) {
  let bound = Path::new(bound_path);
  for dir in dirs {
    if !is_real_dir(dir) {
      continue;
    }
    let link = dir.join(file_name);
    if link == bound {
      continue;
    }
    match std::fs::symlink_metadata(&link) {
      Err(_) => {
        // Absent: create unless the dir itself is missing/unwritable.
        if let Err(err) = std::os::unix::fs::symlink(bound, &link) {
          tracing::debug!("[ipc] Skipping socket link {}: {err}", link.display());
        }
      }
      Ok(meta) => {
        if !meta.file_type().is_symlink() {
          continue; // foreign file/socket: never touch.
        }
        if !link_reclaimable(&link, bound) {
          continue; // live foreign link: never steal.
        }
        let _ = std::fs::remove_file(&link);
        if let Err(err) = std::os::unix::fs::symlink(bound, &link) {
          tracing::debug!("[ipc] Skipping socket link {}: {err}", link.display());
        }
      }
    }
  }
}

/// Remove our fan-out symlinks for `bound_path`: keeps `/tmp` et al. clean
/// across restarts. Best-effort; only links pointing at our socket (or
/// stale ours-shaped ones) are removed — foreign files are never touched.
pub fn remove_socket_links(dirs: &[PathBuf], bound_path: &str) {
  let bound = Path::new(bound_path);
  let Some(file_name) = bound.file_name().and_then(|name| name.to_str()) else {
    return;
  };
  for dir in dirs {
    if !is_real_dir(dir) {
      continue;
    }
    let link = dir.join(file_name);
    if link == bound {
      continue;
    }
    if link_reclaimable(&link, bound) {
      let _ = std::fs::remove_file(&link);
    }
  }
}

/// `discord-ipc-{n}` file name of a bound socket path (for fan-out links).
#[must_use]
pub fn socket_file_name(bound_path: &str) -> String {
  Path::new(bound_path)
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or("discord-ipc-0")
    .to_string()
}

#[cfg(test)]
mod tests {
  use std::ffi::CString;
  use std::fs::Permissions;
  use std::os::unix::ffi::OsStrExt;
  use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
  use std::sync::Mutex;

  use super::*;

  /// Serializes the two tests that use the uid-qualified `/tmp` fallback.
  static FALLBACK_LOCK: Mutex<()> = Mutex::new(());

  /// Isolated 0700 temp dir per test (tag-suffixed, hermetic). The temp base
  /// is canonicalized so platform symlinks (macOS `/var`) keep the chain real.
  fn scratch(tag: &str) -> PathBuf {
    let base = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(format!("adsum-paths-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
      .mode(0o700)
      .create(&dir)
      .expect("scratch dir");
    dir
  }

  /// Fanout skips links whose socket answers (live foreign owner).
  #[test]
  fn fanout_never_steals_live_foreign_link() {
    let ours = scratch("ours");
    let foreign = scratch("foreign");
    // A live foreign service owns discord-ipc-0 here (real socket file
    // behind a symlink): fan-out must leave it alone, never repoint it
    // at our socket.
    let foreign_socket = foreign.join("discord-ipc-9");
    std::fs::write(&foreign_socket, b"socket").expect("foreign socket");
    let foreign_link = foreign.join("discord-ipc-0");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&foreign_socket, &foreign_link).expect("foreign link");

    let bound = ours.join("discord-ipc-0");
    std::fs::write(&bound, b"socket").expect("bound socket");
    fanout_socket_link(
      std::slice::from_ref(&foreign),
      bound.to_str().expect("utf8"),
      "discord-ipc-0",
    );

    assert_eq!(
      std::fs::read_link(&foreign_link).expect("link still there"),
      foreign_socket,
      "live foreign link must not be repointed"
    );
    let _ = std::fs::remove_dir_all(&ours);
    let _ = std::fs::remove_dir_all(&foreign);
  }

  /// Dangling links to dead sockets are reclaimed for our bind.
  #[test]
  fn fanout_reclaims_stale_dangling_link() {
    let dir = scratch("stale");
    // Our previous run crashed and left a dangling ours-shaped link:
    // reclaim it for the live socket.
    let link = dir.join("discord-ipc-0");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("discord-ipc-7"), &link).expect("stale link");
    let bound = dir.join("discord-ipc-1");
    std::fs::write(&bound, b"socket").expect("bound socket");

    fanout_socket_link(
      std::slice::from_ref(&dir),
      bound.to_str().expect("utf8"),
      "discord-ipc-0",
    );
    assert_eq!(std::fs::read_link(&link).expect("link still there"), bound);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// Every supported client layout is discovered per base, in publish order.
  #[test]
  fn socket_alias_dirs_expand_all_layouts_in_order() {
    let base = scratch("alias-all");
    for layout in ALIAS_LAYOUTS {
      if !layout.is_empty() {
        std::fs::create_dir_all(base.join(layout)).expect("alias dir");
      }
    }
    let expected: Vec<PathBuf> = ALIAS_LAYOUTS
      .iter()
      .map(|layout| {
        if layout.is_empty() {
          base.clone()
        } else {
          base.join(*layout)
        }
      })
      .collect();
    assert_eq!(socket_alias_dirs(std::slice::from_ref(&base)), expected);
    let _ = std::fs::remove_dir_all(&base);
  }

  /// Alias discovery keeps base order and drops duplicate directories.
  #[test]
  fn socket_alias_dirs_keep_order_and_dedupe() {
    let a = scratch("alias-order-a");
    let b = scratch("alias-order-b");
    for base in [&a, &b] {
      for layout in ALIAS_LAYOUTS {
        if !layout.is_empty() {
          std::fs::create_dir_all(base.join(layout)).expect("alias dir");
        }
      }
    }
    let dirs = socket_alias_dirs(&[a.clone(), b.clone(), a.clone()]);
    assert_eq!(dirs.first(), Some(&a));
    assert_eq!(dirs.get(ALIAS_LAYOUTS.len()), Some(&b));
    assert_eq!(
      dirs.len(),
      ALIAS_LAYOUTS.len() * 2,
      "repeated bases must not duplicate layouts"
    );
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
  }

  /// Only alias layouts that already exist as real dirs are candidates.
  #[test]
  fn socket_alias_dirs_skip_missing_layouts() {
    let base = scratch("alias-existing");
    let direct = base.clone();
    let flatpak = base.join("app/com.discordapp.Discord");
    let snap = base.join("snap.discord");
    std::fs::create_dir_all(&flatpak).expect("flatpak dir");
    std::fs::create_dir_all(&snap).expect("snap dir");

    let dirs = socket_alias_dirs(std::slice::from_ref(&base));
    assert_eq!(dirs, vec![direct, flatpak, snap]);
    assert!(!base.join("app/dev.vencord.Vesktop").exists());
    let _ = std::fs::remove_dir_all(&base);
  }

  /// A symlinked official client dir is skipped, not followed.
  #[test]
  fn socket_alias_dirs_skip_symlinked_layouts() {
    let base = scratch("alias-symlink");
    let outside = scratch("alias-symlink-outside");
    std::fs::create_dir_all(outside.join("com.discordapp.Discord")).expect("outside official");
    std::fs::create_dir_all(base.join("app")).expect("app dir");
    std::os::unix::fs::symlink(
      outside.join("com.discordapp.Discord"),
      base.join("app/com.discordapp.Discord"),
    )
    .expect("official symlink");
    std::fs::create_dir_all(base.join("snap.discord")).expect("snap dir");

    let dirs = socket_alias_dirs(std::slice::from_ref(&base));
    assert_eq!(dirs, vec![base.clone(), base.join("snap.discord")]);

    let bound = base.join("discord-ipc-0");
    std::fs::write(&bound, b"socket").expect("bound socket");
    fanout_socket_link(&dirs, bound.to_str().expect("utf8"), "discord-ipc-0");
    assert!(
      std::fs::symlink_metadata(outside.join("com.discordapp.Discord/discord-ipc-0")).is_err(),
      "fanout must not follow the symlinked official dir"
    );
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
  }

  /// A symlinked `app/` parent is skipped, not followed.
  #[test]
  fn socket_alias_dirs_skip_symlinked_app_parent() {
    let base = scratch("alias-app-symlink");
    let outside = scratch("alias-app-symlink-outside");
    std::fs::create_dir_all(outside.join("com.discordapp.Discord")).expect("outside official");
    std::os::unix::fs::symlink(&outside, base.join("app")).expect("app symlink");

    let dirs = socket_alias_dirs(std::slice::from_ref(&base));
    assert_eq!(dirs, vec![base.clone()]);
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&outside);
  }

  /// A symlinked base is skipped entirely, not followed.
  #[test]
  fn socket_alias_dirs_skip_symlinked_base() {
    let real = scratch("alias-base-real");
    let link_parent = scratch("alias-base-link");
    let base = link_parent.join("runtime");
    std::fs::create_dir_all(real.join("app/com.discordapp.Discord")).expect("real official");
    std::os::unix::fs::symlink(&real, &base).expect("base symlink");

    let dirs = socket_alias_dirs(std::slice::from_ref(&base));
    assert!(dirs.is_empty(), "symlinked base must not yield aliases");
    let _ = std::fs::remove_dir_all(&real);
    let _ = std::fs::remove_dir_all(&link_parent);
  }

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
    let err =
      prepare_socket_dir(&base.join("app/not-discord")).expect_err("wrong layout must fail");
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

  /// Fanout leaves a foreign regular file squatting the link name alone.
  #[test]
  fn fanout_preserves_foreign_regular_file() {
    let ours = scratch("fanout-file-ours");
    let foreign = scratch("fanout-file-foreign");
    let squat = foreign.join("discord-ipc-0");
    std::fs::write(&squat, b"keep me").expect("foreign file");
    let bound = ours.join("discord-ipc-0");
    std::fs::write(&bound, b"socket").expect("bound socket");

    fanout_socket_link(
      std::slice::from_ref(&foreign),
      bound.to_str().expect("utf8"),
      "discord-ipc-0",
    );
    assert_eq!(
      std::fs::read(&squat).expect("foreign file kept"),
      b"keep me"
    );
    assert!(
      std::fs::symlink_metadata(&squat)
        .expect("metadata")
        .file_type()
        .is_file()
    );
    let _ = std::fs::remove_dir_all(&ours);
    let _ = std::fs::remove_dir_all(&foreign);
  }

  /// Fanout never replaces a live foreign socket with one of our links.
  #[test]
  fn fanout_preserves_live_foreign_socket() {
    let ours = scratch("fanout-sock-ours");
    let foreign = scratch("fanout-sock-foreign");
    let squat = foreign.join("discord-ipc-0");
    let listener = std::os::unix::net::UnixListener::bind(&squat).expect("foreign socket");
    let bound = ours.join("discord-ipc-0");
    std::fs::write(&bound, b"socket").expect("bound socket");

    fanout_socket_link(
      std::slice::from_ref(&foreign),
      bound.to_str().expect("utf8"),
      "discord-ipc-0",
    );
    assert!(
      std::fs::symlink_metadata(&squat)
        .expect("metadata")
        .file_type()
        .is_socket()
    );
    drop(listener);
    let _ = std::fs::remove_dir_all(&ours);
    let _ = std::fs::remove_dir_all(&foreign);
  }

  /// A symlinked alias dir is never followed during fan-out.
  #[test]
  fn fanout_skips_symlinked_directories() {
    let ours = scratch("fanout-symlink-ours");
    let outside = scratch("fanout-symlink-outside");
    let parent = scratch("fanout-symlink-parent");
    let alias = parent.join("alias");
    std::os::unix::fs::symlink(&outside, &alias).expect("alias symlink");
    let bound = ours.join("discord-ipc-0");
    std::fs::write(&bound, b"socket").expect("bound socket");

    fanout_socket_link(
      std::slice::from_ref(&alias),
      bound.to_str().expect("utf8"),
      "discord-ipc-0",
    );
    assert!(
      std::fs::symlink_metadata(outside.join("discord-ipc-0")).is_err(),
      "fanout must not follow a symlinked dir"
    );
    let _ = std::fs::remove_dir_all(&ours);
    let _ = std::fs::remove_dir_all(&outside);
    let _ = std::fs::remove_dir_all(&parent);
  }

  /// Cleanup removes our links only and keeps foreign files, sockets and links.
  #[test]
  fn remove_socket_links_removes_only_ours() {
    let ours = scratch("cleanup-ours");
    let alias = scratch("cleanup-alias");
    let foreign = scratch("cleanup-foreign");
    let bound = ours.join("discord-ipc-0");
    std::fs::write(&bound, b"socket").expect("bound socket");
    let our_link = alias.join("discord-ipc-0");
    std::os::unix::fs::symlink(&bound, &our_link).expect("our link");
    let foreign_file = foreign.join("discord-ipc-0");
    std::fs::write(&foreign_file, b"foreign").expect("foreign file");
    let foreign_target = foreign.join("discord-ipc-9");
    std::fs::write(&foreign_target, b"foreign socket").expect("foreign target");
    let foreign_link = foreign.join("discord-ipc-1");
    std::os::unix::fs::symlink(&foreign_target, &foreign_link).expect("foreign link");

    remove_socket_links(
      &[alias.clone(), foreign.clone()],
      bound.to_str().expect("utf8"),
    );
    assert!(
      std::fs::symlink_metadata(&our_link).is_err(),
      "our link removed"
    );
    assert!(bound.exists(), "bound socket stays");
    assert!(foreign_file.exists(), "foreign file stays");
    assert_eq!(
      std::fs::read_link(&foreign_link).expect("foreign link stays"),
      foreign_target
    );
    let _ = std::fs::remove_dir_all(&ours);
    let _ = std::fs::remove_dir_all(&alias);
    let _ = std::fs::remove_dir_all(&foreign);
  }

  /// Cleanup never follows a symlinked alias dir to remove links elsewhere.
  #[test]
  fn remove_socket_links_skips_symlinked_directories() {
    let ours = scratch("cleanup-symlink-ours");
    let outside = scratch("cleanup-symlink-outside");
    let parent = scratch("cleanup-symlink-parent");
    let alias = parent.join("alias");
    std::os::unix::fs::symlink(&outside, &alias).expect("alias symlink");
    let bound = ours.join("discord-ipc-0");
    std::fs::write(&bound, b"socket").expect("bound socket");
    let outside_link = outside.join("discord-ipc-0");
    std::os::unix::fs::symlink(&bound, &outside_link).expect("outside link");

    remove_socket_links(&[alias], bound.to_str().expect("utf8"));
    assert_eq!(
      std::fs::read_link(&outside_link).expect("outside link stays"),
      bound
    );
    let _ = std::fs::remove_dir_all(&ours);
    let _ = std::fs::remove_dir_all(&outside);
    let _ = std::fs::remove_dir_all(&parent);
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

  /// Shared temp roots are accepted only under their sticky-bit policy.
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
      validate_shared_temp(var_tmp).expect("/var/tmp must pass when present");
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

  /// The fallback dir under `/tmp` is created 0700 and owned by us.
  #[test]
  fn prepare_fallback_dir_creates_private_dir_under_tmp() {
    let tmp = Path::new("/tmp");
    if !tmp.is_dir() {
      return;
    }
    let euid = current_euid();
    let fallback = std::fs::canonicalize(tmp)
      .map(|root| root.join(fallback_dir_name(euid)))
      .unwrap_or_else(|_| tmp.join(fallback_dir_name(euid)));
    let _guard = FALLBACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if !fallback.exists() {
      let dir =
        prepare_fallback_dir(&[tmp.to_path_buf()], euid).expect("prepare fallback dir under /tmp");
      assert_eq!(dir, fallback);
      let meta = std::fs::symlink_metadata(&dir).expect("fallback metadata");
      assert_eq!(meta.mode() & 0o777, 0o700);
      assert_eq!(meta.uid(), euid);
      let _ = std::fs::remove_dir(&dir);
    }

    let unique = tmp.join(format!(".adsum-ipc-test-{}", std::process::id()));
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
}
