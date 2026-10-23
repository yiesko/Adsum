use super::*;

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
