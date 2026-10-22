//! Unix socket placement: Flatpak-preferred bind, conventional bases, and
//! fan-out symlinks for every known client layout. Helpers are best-effort
//! and never touch foreign files.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

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
      if !dirs.contains(&dir) {
        dirs.push(dir);
      }
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

/// Create the official Flatpak socket dir and its `app/` parent without
/// following symlinks; `dir` must be `<runtime>/app/<official-id>`.
pub(crate) fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
  if dir.file_name() != Some(OsStr::new(FLATPAK_DISCORD_APP_ID)) {
    return Err(not_a_real_dir(dir, "not the official Flatpak dir"));
  }
  let app = dir
    .parent()
    .filter(|path| path.file_name() == Some(OsStr::new("app")))
    .ok_or_else(|| not_a_real_dir(dir, "missing app parent"))?;
  let base = app
    .parent()
    .ok_or_else(|| not_a_real_dir(app, "missing runtime base"))?;
  require_real_dir(base)?;
  let canonical = std::fs::canonicalize(base)?;
  if is_shared_temp(&canonical) {
    return Err(not_a_real_dir(base, "shared temp base"));
  }
  ensure_real_dir(app)?;
  ensure_real_dir(dir)
}

/// Whether `canonical` is `/tmp` or `/var/tmp`, resolving platform symlinks
/// (macOS maps both into `/private`).
fn is_shared_temp(canonical: &Path) -> bool {
  ["/tmp", "/var/tmp"]
    .iter()
    .any(|shared| std::fs::canonicalize(shared).is_ok_and(|resolved| resolved == canonical))
}

/// Require `path` to be an existing real directory, refusing symlinks.
fn require_real_dir(path: &Path) -> io::Result<()> {
  let meta = std::fs::symlink_metadata(path)?;
  if meta.file_type().is_symlink() || !meta.is_dir() {
    return Err(not_a_real_dir(path, "not a real directory"));
  }
  Ok(())
}

/// Ensure `path` is a real directory, creating it when absent; symlinks
/// and non-directories are refused so creation never escapes the base.
fn ensure_real_dir(path: &Path) -> io::Result<()> {
  match require_real_dir(path) {
    Ok(()) => Ok(()),
    Err(err) if err.kind() == io::ErrorKind::NotFound => match std::fs::create_dir(path) {
      Ok(()) => Ok(()),
      Err(err) if err.kind() == io::ErrorKind::AlreadyExists => require_real_dir(path),
      Err(err) => Err(err),
    },
    Err(err) => Err(err),
  }
}

/// Build the refusal error for a path that cannot be used as a real dir.
fn not_a_real_dir(path: &Path, reason: &str) -> io::Error {
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
  use std::os::unix::fs::FileTypeExt;

  use super::*;

  /// Isolated temp dir per test (tag-suffixed, hermetic).
  fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("adsum-paths-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
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
}
