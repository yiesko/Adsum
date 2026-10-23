use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

use super::*;

/// Isolated temp dir per test (tag-suffixed, hermetic). The base is
/// canonicalized so macOS `$TMPDIR` stays inside the 104-byte `sun_path`.
fn scratch(tag: &str) -> std::path::PathBuf {
  let base = std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::env::temp_dir());
  let dir = base.join(format!("adsum-probe-test-{}-{tag}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).expect("scratch dir");
  dir
}

/// An absent path is `Missing` with no identity.
#[test]
fn missing_path_is_missing() {
  let dir = scratch("missing");
  let probe = probe_socket(&dir.join("discord-ipc-9"));
  assert_eq!(probe.state, SocketState::Missing);
  assert_eq!(probe.identity, None);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A held listener accepts the probe: `Active` plus its identity.
#[test]
fn live_listener_is_active() {
  let dir = scratch("active");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  let probe = probe_socket(&path);
  assert_eq!(probe.state, SocketState::Active);
  assert!(probe.identity.is_some(), "socket identity must be recorded");
  drop(listener);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A bound listener that never accepts is still `Active` (no PING needed).
#[test]
fn silent_listener_is_active() {
  let dir = scratch("silent");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  // No accept loop and no PING/PONG: the kernel completes the connect
  // into the backlog, which is enough to keep the index occupied.
  assert_eq!(probe_socket(&path).state, SocketState::Active);
  drop(listener);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A socket whose listener is gone but whose path remains is `Stale`.
#[test]
fn refused_socket_is_stale() {
  let dir = scratch("stale");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  let meta = std::fs::symlink_metadata(&path).expect("socket metadata");
  let expected = FileId {
    dev: meta.dev(),
    ino: meta.ino(),
  };
  drop(listener);

  let probe = probe_socket(&path);
  assert_eq!(probe.state, SocketState::Stale);
  assert_eq!(probe.identity, Some(expected));
  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_dir_all(&dir);
}

/// Symlinks are not followed; files and dirs are not sockets.
#[test]
fn symlink_regular_file_and_directory_are_not_sockets() {
  let dir = scratch("not-socket");
  let target = dir.join("target");
  let listener = UnixListener::bind(&target).expect("target socket");
  let link = dir.join("discord-ipc-0");
  std::os::unix::fs::symlink(&target, &link).expect("socket symlink");
  let file = dir.join("discord-ipc-1");
  std::fs::write(&file, b"not a socket").expect("regular file");

  for path in [&link, &file, &dir] {
    let probe = probe_socket(path);
    assert_eq!(
      probe.state,
      SocketState::NotSocket,
      "path {}",
      path.display()
    );
    assert_eq!(probe.identity, None, "non-sockets carry no identity");
  }
  drop(listener);
  let _ = std::fs::remove_dir_all(&dir);
}

/// An unreadable path is `Unknown`, never `Stale`.
#[test]
fn permission_error_is_unknown() {
  if crate::paths::current_euid() == 0 {
    return; // root bypasses the permission check.
  }
  let dir = scratch("permission");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  std::fs::set_permissions(&dir, Permissions::from_mode(0o000)).expect("chmod 0000");
  let probe = probe_socket(&path);
  std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).expect("restore 0700");
  assert_eq!(probe.state, SocketState::Unknown);
  assert_eq!(probe.identity, None);
  drop(listener);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A live listener rebound at the same path survives a stale identity unlink.
#[test]
fn unlink_socket_if_unchanged_preserves_replaced_live_socket() {
  let dir = scratch("unlink-live");
  let path = dir.join("discord-ipc-0");
  let stale = UnixListener::bind(&path).expect("stale listener");
  let old_id = probe_socket(&path).identity.expect("stale identity");
  // Bind the replacement while the stale inode is still allocated, then
  // move it into place: inode reuse cannot make the identities collide.
  let live_path = dir.join("discord-ipc-live");
  let live = UnixListener::bind(&live_path).expect("live listener");
  drop(stale);
  std::fs::remove_file(&path).expect("remove stale path");
  std::fs::rename(&live_path, &path).expect("move live socket into place");
  assert_ne!(probe_socket(&path).identity, Some(old_id));

  assert!(!unlink_socket_if_unchanged(&path, old_id));
  assert!(
    std::fs::symlink_metadata(&path)
      .expect("live socket kept")
      .file_type()
      .is_socket()
  );
  std::os::unix::net::UnixStream::connect(&path).expect("live socket accepts");

  drop(live);
  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A fresh probe keeps a live listener even when the identity matches.
#[test]
fn stale_recheck_preserves_live_socket_with_matching_identity() {
  let dir = scratch("recheck-live");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  let identity = probe_socket(&path).identity.expect("socket identity");

  assert!(!unlink_stale_socket_if_unchanged(&path, identity));
  assert!(
    std::fs::symlink_metadata(&path).is_ok(),
    "live socket must stay"
  );

  drop(listener);
  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_dir_all(&dir);
}

/// After the listener drops, the matching stale socket is removed.
#[test]
fn stale_recheck_removes_matching_stale_socket() {
  let dir = scratch("recheck-stale");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  let identity = probe_socket(&path).identity.expect("socket identity");
  drop(listener);
  assert_eq!(probe_socket(&path).state, SocketState::Stale);

  assert!(unlink_stale_socket_if_unchanged(&path, identity));
  assert!(std::fs::symlink_metadata(&path).is_err());

  let _ = std::fs::remove_dir_all(&dir);
}

/// A stale socket with a different inode is preserved by the recheck.
#[test]
fn stale_recheck_preserves_replaced_identity() {
  let dir = scratch("recheck-replaced");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  let old_id = probe_socket(&path).identity.expect("socket identity");
  let replacement_path = dir.join("discord-ipc-live");
  let replacement = UnixListener::bind(&replacement_path).expect("replacement listener");
  drop(listener);
  std::fs::remove_file(&path).expect("remove stale path");
  std::fs::rename(&replacement_path, &path).expect("move replacement into place");
  drop(replacement);
  assert_eq!(probe_socket(&path).state, SocketState::Stale);
  assert_ne!(probe_socket(&path).identity, Some(old_id));

  assert!(!unlink_stale_socket_if_unchanged(&path, old_id));
  assert!(
    std::fs::symlink_metadata(&path).is_ok(),
    "replaced socket must stay"
  );

  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A replaced entry survives; a matching identity is removed.
#[test]
fn unlink_socket_if_unchanged_preserves_replaced_entry() {
  let dir = scratch("unlink");
  let path = dir.join("discord-ipc-0");
  let listener = UnixListener::bind(&path).expect("listener");
  let identity = probe_socket(&path).identity.expect("socket identity");
  drop(listener);

  std::fs::remove_file(&path).expect("remove stale socket");
  std::fs::write(&path, b"replacement").expect("replacement file");
  assert!(!unlink_socket_if_unchanged(&path, identity));
  assert_eq!(
    std::fs::read(&path).expect("replacement kept"),
    b"replacement"
  );

  std::fs::remove_file(&path).expect("remove replacement");
  let listener = UnixListener::bind(&path).expect("second listener");
  let identity = probe_socket(&path).identity.expect("socket identity");
  drop(listener);
  assert!(unlink_socket_if_unchanged(&path, identity));
  assert!(std::fs::symlink_metadata(&path).is_err());
  let _ = std::fs::remove_dir_all(&dir);
}
