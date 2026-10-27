//! Subscription wire format and cleanup: the kernel decrements its
//! listener count only on IGNORE, never when the socket closes.

use super::*;

/// A datagram socketpair whose peer keeps the test from touching netlink.
fn socketpair() -> (i32, i32) {
  let mut fds = [0i32; 2];
  // SAFETY: `fds` has room for both descriptors; socketpair fills them.
  let rc = unsafe {
    libc::socketpair(
      libc::AF_UNIX,
      libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
      0,
      fds.as_mut_ptr(),
    )
  };
  assert_eq!(
    rc,
    0,
    "socketpair failed: {}",
    std::io::Error::last_os_error()
  );
  (fds[0], fds[1])
}

#[test]
fn request_carries_the_multicast_operation() {
  let message = mcast_request(PROC_CN_MCAST_IGNORE);
  let total = MCAST_REQUEST_LEN as u32;
  assert_eq!(u32::from_le_bytes(message[0..4].try_into().unwrap()), total);
  assert_eq!(
    u16::from_le_bytes(message[4..6].try_into().unwrap()),
    NLMSG_MIN_TYPE
  );
  assert_eq!(
    u16::from_le_bytes(message[6..8].try_into().unwrap()),
    NLM_F_REQUEST
  );
  let cn = SIZE_NLMSGHDR;
  assert_eq!(
    u32::from_le_bytes(message[cn..cn + 4].try_into().unwrap()),
    CN_IDX_PROC
  );
  assert_eq!(
    u32::from_le_bytes(message[cn + 4..cn + 8].try_into().unwrap()),
    CN_VAL_PROC
  );
  assert_eq!(
    u16::from_le_bytes(message[cn + 16..cn + 18].try_into().unwrap()),
    4
  );
  assert_eq!(
    u32::from_le_bytes(message[cn + 20..cn + 24].try_into().unwrap()),
    PROC_CN_MCAST_IGNORE
  );
}

/// One `NLMSG_ERROR` datagram carrying `code`; the refusal walk reads the
/// first four body bytes.
fn error_frame(code: i32) -> Vec<u8> {
  let total = SIZE_NLMSGHDR + 4;
  let mut frame = vec![0u8; total];
  frame[0..4].copy_from_slice(&(total as u32).to_le_bytes());
  frame[4..6].copy_from_slice(&NLMSG_ERROR.to_le_bytes());
  frame[SIZE_NLMSGHDR..].copy_from_slice(&code.to_le_bytes());
  frame
}

/// Read one datagram without blocking; a missing frame fails the test.
fn recv_frame(fd: i32) -> Vec<u8> {
  let mut buf = [0u8; 128];
  // SAFETY: `fd` is open and `buf` has room for one frame.
  let received = unsafe {
    libc::recv(
      fd,
      buf.as_mut_ptr() as *mut libc::c_void,
      buf.len(),
      libc::MSG_DONTWAIT,
    )
  };
  assert!(
    received > 0,
    "expected a frame: {}",
    std::io::Error::last_os_error()
  );
  buf[..received as usize].to_vec()
}

#[test]
fn refusal_after_listen_still_unsubscribes() {
  let (local, peer) = socketpair();
  // The refusal is already queued when the subscription reads its ACK.
  let refusal = error_frame(-libc::EPERM);
  // SAFETY: `peer` is open and `refusal` outlives the call.
  let sent = unsafe {
    libc::send(
      peer,
      refusal.as_ptr() as *const libc::c_void,
      refusal.len(),
      0,
    )
  };
  assert_eq!(sent, refusal.len() as isize);
  let result = subscribe_bound(local);
  assert!(result.is_err(), "the refusal must abort the subscription");
  // The peer sees LISTEN first, then the IGNORE the guard sends on drop.
  let cn = SIZE_NLMSGHDR;
  let listen = recv_frame(peer);
  assert_eq!(
    u32::from_le_bytes(listen[cn + 20..cn + 24].try_into().unwrap()),
    PROC_CN_MCAST_LISTEN
  );
  let ignore = recv_frame(peer);
  assert_eq!(
    u32::from_le_bytes(ignore[cn + 20..cn + 24].try_into().unwrap()),
    PROC_CN_MCAST_IGNORE
  );
  // SAFETY: `peer` is still owned by this test.
  unsafe {
    libc::close(peer);
  }
}

#[test]
fn drop_sends_ignore_before_closing() {
  let (fd, peer) = socketpair();
  drop(Subscription::new(fd));
  // The guard must also close the descriptor, not only unsubscribe:
  // `fcntl` on a closed fd fails with EBADF. Checked before the recv
  // below to keep the fd-reuse window as small as possible.
  // SAFETY: `fd` is a plain integer; `fcntl` on a closed descriptor is safe.
  let closed = unsafe { libc::fcntl(fd, libc::F_GETFD) };
  let err = std::io::Error::last_os_error();
  assert_eq!(closed, -1, "drop must close the socket: {err}");
  assert_eq!(err.raw_os_error(), Some(libc::EBADF));
  let mut buf = [0u8; SIZE_NLMSGHDR + SIZE_CN_MSG + 4];
  // SAFETY: `peer` is open and `buf` has room for the whole frame.
  let received = unsafe {
    libc::recv(
      peer,
      buf.as_mut_ptr() as *mut libc::c_void,
      buf.len(),
      libc::MSG_DONTWAIT,
    )
  };
  assert!(
    received > 0,
    "drop must send one IGNORE frame: {}",
    std::io::Error::last_os_error()
  );
  assert_eq!(received as usize, buf.len(), "exactly one frame");
  let cn = SIZE_NLMSGHDR;
  assert_eq!(
    u32::from_le_bytes(buf[cn + 20..cn + 24].try_into().unwrap()),
    PROC_CN_MCAST_IGNORE
  );
  // SAFETY: `peer` is still owned by this test.
  unsafe {
    libc::close(peer);
  }
}
