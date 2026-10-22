//! Socket probe: classify a path as missing, active, stale, or unknown.
//!
//! The probe never follows symlinks and never sends a `PING`: a successful
//! connect proves a listener holds the socket, so only `ECONNREFUSED`
//! authorizes stale reclamation.

use std::io;
use std::mem::{MaybeUninit, offset_of};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

/// Budget for one nonblocking connect to reach a verdict.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Outcome class of a socket path probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SocketState {
  /// Nothing exists at the path.
  Missing,
  /// A listener accepts connections.
  Active,
  /// A socket exists but refuses connections.
  Stale,
  /// The probe could not reach a safe verdict.
  Unknown,
  /// The entry is a symlink, file, directory, or other non-socket.
  NotSocket,
}

/// Filesystem identity of an entry, used to revalidate before unlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
  /// Device number of the containing filesystem.
  pub dev: u64,
  /// Inode number within that filesystem.
  pub ino: u64,
}

/// Probe result: state plus the socket identity when the entry is a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SocketProbe {
  /// Classification of the entry.
  pub state: SocketState,
  /// `(dev, ino)` for sockets; `None` for every other state.
  pub identity: Option<FileId>,
}

/// Classify the entry at `path` without following symlinks.
#[must_use]
pub(crate) fn probe_socket(path: &Path) -> SocketProbe {
  let meta = match std::fs::symlink_metadata(path) {
    Ok(meta) => meta,
    Err(err) if err.kind() == io::ErrorKind::NotFound => {
      return SocketProbe {
        state: SocketState::Missing,
        identity: None,
      };
    }
    Err(_) => {
      return SocketProbe {
        state: SocketState::Unknown,
        identity: None,
      };
    }
  };
  if !meta.file_type().is_socket() {
    return SocketProbe {
      state: SocketState::NotSocket,
      identity: None,
    };
  }
  SocketProbe {
    state: connect_state(path),
    identity: Some(FileId {
      dev: meta.dev(),
      ino: meta.ino(),
    }),
  }
}

/// Remove `path` only when it is still the socket identified by `identity`.
#[must_use]
pub(crate) fn unlink_socket_if_unchanged(path: &Path, identity: FileId) -> bool {
  let Ok(meta) = std::fs::symlink_metadata(path) else {
    return false;
  };
  if !meta.file_type().is_socket() || meta.dev() != identity.dev || meta.ino() != identity.ino {
    return false;
  }
  std::fs::remove_file(path).is_ok()
}

/// Nonblocking connect verdict for a path already known to be a socket.
fn connect_state(path: &Path) -> SocketState {
  let Ok(addr) = unix_addr(path) else {
    return SocketState::Unknown;
  };
  // SAFETY: `AF_UNIX`/`SOCK_STREAM` are valid socket parameters.
  let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
  if fd < 0 {
    return SocketState::Unknown;
  }
  // SAFETY: `fd` is a fresh descriptor owned by this function; `OwnedFd`
  // closes it on every return path.
  let fd = unsafe { OwnedFd::from_raw_fd(fd) };
  if set_nonblocking_cloexec(fd.as_raw_fd()).is_err() {
    return SocketState::Unknown;
  }
  // SAFETY: `addr` holds an initialized `sockaddr_un` and `addr.len` is its
  // exact length; `fd` is a valid socket.
  let result = unsafe {
    libc::connect(
      fd.as_raw_fd(),
      std::ptr::from_ref(&addr.sockaddr).cast::<libc::sockaddr>(),
      addr.len,
    )
  };
  if result == 0 {
    return SocketState::Active;
  }
  match io::Error::last_os_error().raw_os_error() {
    // AF_UNIX reports a dead listener synchronously, not via poll.
    Some(libc::ECONNREFUSED) => SocketState::Stale,
    Some(libc::EINPROGRESS | libc::EAGAIN) => wait_for_connect(fd.as_raw_fd()),
    _ => SocketState::Unknown,
  }
}

/// Poll a nonblocking connect with the shared budget, then read `SO_ERROR`.
fn wait_for_connect(fd: RawFd) -> SocketState {
  let deadline = Instant::now() + CONNECT_TIMEOUT;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      return SocketState::Unknown;
    }
    let timeout = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
    let mut poll_fd = libc::pollfd {
      fd,
      events: libc::POLLOUT,
      revents: 0,
    };
    // SAFETY: `poll_fd` is initialized and valid for one entry.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout) };
    if ready == 0 {
      return SocketState::Unknown; // timed out
    }
    if ready < 0 {
      if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
        continue;
      }
      return SocketState::Unknown;
    }
    let mut error: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is a valid socket; the value and length pointers refer to
    // writable locals of the right types.
    let result = unsafe {
      libc::getsockopt(
        fd,
        libc::SOL_SOCKET,
        libc::SO_ERROR,
        std::ptr::from_mut(&mut error).cast(),
        &mut len,
      )
    };
    if result != 0 {
      return SocketState::Unknown;
    }
    return match error {
      0 => SocketState::Active,
      libc::ECONNREFUSED => SocketState::Stale,
      _ => SocketState::Unknown,
    };
  }
}

/// Set `O_NONBLOCK` and `FD_CLOEXEC` on a freshly created socket.
fn set_nonblocking_cloexec(fd: RawFd) -> io::Result<()> {
  // SAFETY: `fd` is a valid descriptor owned by the caller.
  let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
  if status < 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: same valid descriptor; `F_SETFL` takes the flags word.
  if unsafe { libc::fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK) } < 0 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: same valid descriptor; `F_GETFD`/`F_SETFD` take descriptor flags.
  let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
  if flags < 0 {
    return Err(io::Error::last_os_error());
  }
  if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

/// Initialized `sockaddr_un` plus the exact length passed to `connect`.
struct UnixAddr {
  sockaddr: libc::sockaddr_un,
  len: libc::socklen_t,
}

/// Build a `sockaddr_un` for `path`, refusing NUL bytes and overflow.
fn unix_addr(path: &Path) -> io::Result<UnixAddr> {
  let bytes = path.as_os_str().as_bytes();
  // SAFETY: `sockaddr_un` is plain C data for which all-zero is valid.
  let mut sockaddr = unsafe { MaybeUninit::<libc::sockaddr_un>::zeroed().assume_init() };
  if bytes.is_empty() || bytes.len() >= sockaddr.sun_path.len() || bytes.contains(&0) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "socket path is empty, too long, or contains a NUL byte",
    ));
  }
  sockaddr.sun_family = libc::AF_UNIX as libc::sa_family_t;
  // SAFETY: `sun_path` was proven large enough above and the source slice
  // does not overlap the destination struct.
  unsafe {
    std::ptr::copy_nonoverlapping(
      bytes.as_ptr(),
      sockaddr.sun_path.as_mut_ptr().cast::<u8>(),
      bytes.len(),
    );
  }
  let len = offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
  #[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  {
    sockaddr.sun_len = len as u8;
  }
  Ok(UnixAddr {
    sockaddr,
    len: len as libc::socklen_t,
  })
}

#[cfg(test)]
mod tests {
  use std::fs::Permissions;
  use std::os::unix::fs::PermissionsExt;
  use std::os::unix::net::UnixListener;

  use super::*;

  /// Isolated temp dir per test (tag-suffixed, hermetic).
  fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("adsum-probe-test-{}-{tag}", std::process::id()));
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
}
