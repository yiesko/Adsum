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

/// Remove `path` only when a fresh probe still reports the same stale socket.
#[must_use]
pub(crate) fn unlink_stale_socket_if_unchanged(path: &Path, identity: FileId) -> bool {
  let probe = probe_socket(path);
  probe.state == SocketState::Stale
    && probe.identity == Some(identity)
    && unlink_socket_if_unchanged(path, identity)
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
#[path = "../tests/unit/probe.rs"]
mod tests;
