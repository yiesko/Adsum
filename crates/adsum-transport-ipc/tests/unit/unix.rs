use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};

use super::*;

/// Isolated 0700 temp dir per test, under canonical `/tmp` when available:
/// macOS `$TMPDIR` is long and `sun_path` caps socket paths at 104 bytes.
fn scratch(tag: &str) -> PathBuf {
  let base = std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::env::temp_dir());
  let dir = base.join(format!("adsum-unix-test-{}-{tag}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::DirBuilder::new()
    .mode(0o700)
    .create(&dir)
    .expect("scratch dir");
  dir
}

/// A final path already occupied at install time is rejected and preserved.
#[test]
fn finish_bind_rejects_replaced_socket_path() {
  let dir = scratch("finish-bind-replaced");
  let path = dir.join("discord-ipc-0");
  let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
  let foreign = UnixListener::bind(&path).expect("foreign listener");
  let foreign_mode = std::fs::symlink_metadata(&path)
    .expect("foreign metadata")
    .mode()
    & 0o777;

  assert!(
    finish_bind(listener, &temp, &path).is_none(),
    "a path already naming another socket must be rejected"
  );
  assert!(
    std::fs::symlink_metadata(&path)
      .expect("foreign socket kept")
      .file_type()
      .is_socket(),
    "the foreign socket must stay at the path"
  );
  assert_eq!(
    std::fs::symlink_metadata(&path)
      .expect("foreign metadata")
      .mode()
      & 0o777,
    foreign_mode,
    "finish_bind must not chmod a foreign socket"
  );
  UnixStream::connect(&path).expect("foreign socket still accepts connections");
  assert!(
    std::fs::symlink_metadata(&temp).is_err(),
    "the private temp name must be cleaned up"
  );

  drop(foreign);
  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A handle opened before the temp name is replaced must keep chmod/link
/// on the original socket, never on the replacement.
#[cfg(target_os = "linux")]
#[test]
fn finish_bind_from_handle_ignores_replaced_temp_name() {
  let dir = scratch("finish-bind-handle-race");
  let path = dir.join("discord-ipc-0");
  let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
  let handle = open_socket_handle(&temp).expect("socket handle");
  let original = socket_identity_from_fd(handle.as_fd()).expect("original identity");

  std::fs::remove_file(&temp).expect("remove temp name");
  let foreign = UnixListener::bind(&temp).expect("foreign listener");
  std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o666)).expect("foreign mode");
  let foreign_id = socket_identity(&temp).expect("foreign identity");
  assert_ne!(original, foreign_id, "the replacement must be distinct");

  let result = finish_bind_from_handle(listener, &temp, &path, handle);

  let foreign_mode = std::fs::symlink_metadata(&temp)
    .expect("foreign name must be preserved")
    .mode()
    & 0o777;
  assert_eq!(foreign_mode, 0o666, "foreign socket mode must not change");

  match result {
    Some((_, socket_path, identity)) => {
      assert_eq!(socket_path, path.to_string_lossy().as_ref());
      assert_eq!(identity, original);
      assert_eq!(
        socket_identity(&path),
        Some(original),
        "the final name must carry the original inode"
      );
    }
    None => {
      assert_ne!(
        socket_identity(&path),
        Some(foreign_id),
        "the foreign inode must never be installed"
      );
    }
  }

  drop(foreign);
  let _ = std::fs::remove_file(&temp);
  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_dir_all(&dir);
}

/// A renamed temp socket stays linkable: the fd install must succeed with
/// the original inode and leave the foreign temp name untouched.
#[cfg(target_os = "linux")]
#[test]
fn finish_bind_from_handle_installs_original_after_temp_rename() {
  let dir = scratch("finish-bind-handle-rename");
  let path = dir.join("discord-ipc-0");
  let renamed = dir.join("original-socket");
  let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
  let handle = open_socket_handle(&temp).expect("socket handle");
  let original = socket_identity_from_fd(handle.as_fd()).expect("original identity");

  std::fs::rename(&temp, &renamed).expect("rename temp name");
  let foreign = UnixListener::bind(&temp).expect("foreign listener");
  std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o666)).expect("foreign mode");
  let foreign_id = socket_identity(&temp).expect("foreign identity");
  assert_ne!(original, foreign_id, "the replacement must be distinct");

  let (_, socket_path, identity) =
    finish_bind_from_handle(listener, &temp, &path, handle).expect("fd install must succeed");

  assert_eq!(socket_path, path.to_string_lossy().as_ref());
  assert_eq!(
    identity, original,
    "the installed identity must be original"
  );
  assert_eq!(
    socket_identity(&path),
    Some(original),
    "the final name must carry the original inode"
  );
  assert_eq!(
    std::fs::symlink_metadata(&path)
      .expect("final metadata")
      .mode()
      & 0o777,
    0o600,
    "the original socket must be 0600"
  );
  assert_eq!(
    socket_identity(&temp),
    Some(foreign_id),
    "the foreign temp name must be preserved"
  );
  assert_eq!(
    std::fs::symlink_metadata(&temp)
      .expect("foreign metadata")
      .mode()
      & 0o777,
    0o666,
    "the foreign socket mode must not change"
  );

  drop(foreign);
  let _ = std::fs::remove_file(&temp);
  let _ = std::fs::remove_file(&path);
  let _ = std::fs::remove_file(&renamed);
  let _ = std::fs::remove_dir_all(&dir);
}

/// The temp name must be shorter than `discord-ipc-N`, preserving long paths.
#[test]
fn bind_temp_name_stays_shorter_than_final() {
  let dir = scratch("temp-name-length");
  let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
  let temp_name = temp.file_name().expect("temp name").to_string_lossy();
  assert!(
    temp_name.len() < "discord-ipc-0".len(),
    "temp name {temp_name} must leave room for a full sun_path"
  );

  drop(listener);
  let _ = std::fs::remove_file(&temp);
  let _ = std::fs::remove_dir_all(&dir);
}

/// The prepared Flatpak dir is tried before conventional bases, with
/// shared temp roots filtered out.
#[test]
fn bind_dirs_put_preferred_first() {
  let bases = vec![PathBuf::from("/run/user/1000"), PathBuf::from("/tmp")];
  let preferred = PathBuf::from("/run/user/1000/app/com.discordapp.Discord");
  let dirs = bind_dirs_in_order(bases, Some(&preferred), None);
  assert_eq!(
    dirs,
    vec![
      PathBuf::from("/run/user/1000/app/com.discordapp.Discord"),
      PathBuf::from("/run/user/1000"),
    ]
  );
}

/// Without a prepared Flatpak dir, the conventional order is unchanged.
#[test]
fn bind_dirs_without_preferred_keep_bases() {
  let bases = vec![PathBuf::from("/run/user/1000")];
  assert_eq!(bind_dirs_in_order(bases.clone(), None, None), bases);
}

/// The Flatpak diagnostic is the exact operator-facing contract.
#[test]
fn flatpak_diagnostic_is_exact() {
  assert_eq!(
    FLATPAK_UNAVAILABLE_WARNING,
    "[ipc] Flatpak-compatible socket path unavailable; compatibility aliases may not be reachable from sandboxed clients"
  );
}

/// Compatibility holds only when the bound path is inside a prepared
/// preferred dir; a string prefix is not a path prefix.
#[test]
fn flatpak_compatibility_detects_bound_path() {
  let preferred = Path::new("/run/user/1000/app/com.discordapp.Discord");
  assert!(flatpak_compatibility_available(
    Some(preferred),
    Path::new("/run/user/1000/app/com.discordapp.Discord/discord-ipc-0")
  ));
  assert!(!flatpak_compatibility_available(
    None,
    Path::new("/run/user/1000/app/com.discordapp.Discord/discord-ipc-0")
  ));
  assert!(!flatpak_compatibility_available(
    Some(preferred),
    Path::new("/run/user/1000/discord-ipc-0")
  ));
  assert!(!flatpak_compatibility_available(
    Some(preferred),
    Path::new("/run/user/1000/app/com.discordapp.DiscordCanary/discord-ipc-0")
  ));
}

/// Transient per-connection failures retry at once; persistent ones
/// pause instead of hot-spinning the accept loop.
#[test]
fn accept_errors_back_off_only_when_persistent() {
  assert_eq!(
    accept_error_backoff(&std::io::Error::from(ErrorKind::ConnectionAborted)),
    None
  );
  assert_eq!(
    accept_error_backoff(&std::io::Error::from(ErrorKind::Interrupted)),
    None
  );
  assert_eq!(
    accept_error_backoff(&std::io::Error::from(ErrorKind::PermissionDenied)),
    Some(Duration::from_millis(50))
  );
}
