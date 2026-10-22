//! End-to-end SIGTERM check: the real binary runs the normal graceful
//! shutdown and removes its socket, scratch alias, and ownership marker.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long the child gets to publish its socket, and to exit afterwards.
const WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(25);

/// Private runtime tree; removed on drop even when an assertion fails.
struct Scratch(PathBuf);

impl Scratch {
  /// Create a unique `0700` scratch directory for this test process.
  fn new() -> Self {
    // `/tmp` keeps the preferred Flatpak socket path short enough for
    // macOS's 104-byte `sun_path` and ignores long inherited `TMPDIR`s.
    let root = PathBuf::from("/tmp");
    // Canonicalize so platform symlinks (macOS `/var`) keep the chain real.
    let root = fs::canonicalize(&root).unwrap_or(root);
    let pid = std::process::id();
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock is after the epoch")
      .as_nanos();
    let path = root.join(format!("adsum-sigterm-{pid}-{nanos}"));
    fs::DirBuilder::new()
      .mode(0o700)
      .create(&path)
      .expect("create scratch runtime dir");
    Self(path)
  }

  fn path(&self) -> &Path {
    &self.0
  }
}

impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

/// Kills and reaps the daemon if the test unwinds before it has exited.
struct ChildGuard(Option<Child>);

impl ChildGuard {
  fn new(child: Child) -> Self {
    Self(Some(child))
  }

  fn child(&mut self) -> &mut Child {
    self.0.as_mut().expect("child is held until taken")
  }

  fn take(&mut self) -> Child {
    self.0.take().expect("child is held until taken")
  }
}

impl Drop for ChildGuard {
  fn drop(&mut self) {
    if let Some(mut child) = self.0.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
  }
}

/// Paths that do not exist yet, for timeout and early-exit diagnostics.
fn missing_paths(paths: &[&Path]) -> Vec<String> {
  paths
    .iter()
    .filter(|path| fs::symlink_metadata(path).is_err())
    .map(|path| path.display().to_string())
    .collect()
}

/// Wait until every path exists; a child exit ends the wait with its status.
fn wait_for_paths(paths: &[&Path], child: &mut Child) -> Result<(), ExitStatus> {
  let deadline = Instant::now() + WAIT;
  loop {
    if paths.iter().all(|path| fs::symlink_metadata(path).is_ok()) {
      return Ok(());
    }
    if let Some(status) = child.try_wait().expect("query child status") {
      return Err(status);
    }
    if Instant::now() >= deadline {
      panic!(
        "timed out after {WAIT:?} waiting for {:?}",
        missing_paths(paths)
      );
    }
    thread::sleep(POLL);
  }
}

/// Wait up to `WAIT` for the child to exit; `None` means it is still running.
fn wait_for_exit(child: &mut Child) -> Option<ExitStatus> {
  let deadline = Instant::now() + WAIT;
  loop {
    if let Some(status) = child.try_wait().expect("query child status") {
      return Some(status);
    }
    if Instant::now() >= deadline {
      return None;
    }
    thread::sleep(POLL);
  }
}

/// Wait until every path exists while both children are still running.
fn wait_for_paths_two(paths: &[&Path], first: &mut Child, second: &mut Child) {
  let deadline = Instant::now() + WAIT;
  loop {
    if paths.iter().all(|path| fs::symlink_metadata(path).is_ok()) {
      return;
    }
    if let Some(status) = first.try_wait().expect("query first child status") {
      panic!(
        "first daemon exited with {status} before publishing {:?}",
        missing_paths(paths)
      );
    }
    if let Some(status) = second.try_wait().expect("query second child status") {
      panic!(
        "second daemon exited with {status} before publishing {:?}",
        missing_paths(paths)
      );
    }
    if Instant::now() >= deadline {
      panic!(
        "timed out after {WAIT:?} waiting for {:?}",
        missing_paths(paths)
      );
    }
    thread::sleep(POLL);
  }
}

/// Daemon command with separate home/cache and a pid-derived port block.
fn daemon(runtime: &Path, home: &Path, cache: &Path, base: u32) -> Command {
  let port = |offset: u32| {
    u16::try_from(base + offset)
      .expect("pid-derived test port fits in u16")
      .to_string()
  };
  let mut command = Command::new(env!("CARGO_BIN_EXE_adsum"));
  command
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", home)
    .env("XDG_CONFIG_HOME", home.join("config"))
    .env("XDG_CACHE_HOME", cache)
    .env("XDG_RUNTIME_DIR", runtime)
    .env("TMPDIR", runtime)
    .arg("--no-process-scan")
    .arg("--bridge-port")
    .arg(port(0))
    .arg("--bridge-port-end")
    .arg(port(9))
    .arg("--msgpack-port")
    .arg(port(10))
    .arg("--ws-port-start")
    .arg(port(11))
    .arg("--ws-port-end")
    .arg(port(20))
    .stdin(Stdio::null());
  command
}

/// Send SIGTERM to a still-unreaped child (its pid cannot be recycled).
fn sigterm(child: &mut Child) {
  let pid = libc::pid_t::try_from(child.id()).expect("child pid fits in pid_t");
  // SAFETY: unreaped means still ours: the kernel cannot recycle a PID
  // until the child is waited on, so this signal cannot hit another process.
  let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
  assert_eq!(
    rc,
    0,
    "kill(SIGTERM) failed: {}",
    std::io::Error::last_os_error()
  );
}

/// Connect to `path` and complete the IPC handshake, asserting READY.
fn handshake(path: &Path) {
  let mut stream =
    UnixStream::connect(path).unwrap_or_else(|err| panic!("connect {}: {err}", path.display()));
  stream
    .set_read_timeout(Some(WAIT))
    .expect("handshake timeout");
  let body = br#"{"v":1,"client_id":"sigterm-peer"}"#;
  let len = u32::try_from(body.len()).expect("handshake body fits in u32");
  let mut frame = Vec::with_capacity(8 + body.len());
  frame.extend_from_slice(&0_u32.to_le_bytes());
  frame.extend_from_slice(&len.to_le_bytes());
  frame.extend_from_slice(body);
  stream.write_all(&frame).expect("write handshake");

  let mut header = [0_u8; 8];
  stream.read_exact(&mut header).expect("read reply header");
  let packet_type = u32::from_le_bytes(header[0..4].try_into().expect("packet type"));
  let reply_len = u32::from_le_bytes(header[4..8].try_into().expect("packet length")) as usize;
  let mut reply = vec![0_u8; reply_len];
  stream.read_exact(&mut reply).expect("read reply body");
  assert_eq!(
    packet_type,
    1,
    "expected a frame reply from {}",
    path.display()
  );
  let reply = String::from_utf8(reply).expect("utf8 reply");
  assert!(
    reply.contains("READY"),
    "expected READY from {}, got: {reply}",
    path.display()
  );
}

/// SIGTERM must resolve the same shutdown as Ctrl+C: a clean exit with the
/// bound socket, scratch alias, and marker all removed.
#[test]
fn sigterm_removes_owned_socket_alias_and_marker() {
  let scratch = Scratch::new();
  let runtime = scratch.path();

  // One high, pid-derived port block; no two servers share a port.
  let base = 30000 + (std::process::id() % 20000);
  let cache = runtime.join("cache");
  fs::DirBuilder::new()
    .mode(0o700)
    .create(&cache)
    .expect("create cache dir");

  let preferred = runtime.join("app/com.discordapp.Discord/discord-ipc-0");
  let alias = runtime.join("discord-ipc-0");
  // SAFETY: geteuid has no preconditions and cannot fail.
  let euid = unsafe { libc::geteuid() };
  let marker = runtime.join(format!(".adsum-ipc-owner-{euid}-0"));

  let mut command = daemon(runtime, runtime, &cache, base);
  let mut guard = ChildGuard::new(command.spawn().expect("spawn adsum binary"));
  let paths: [&Path; 3] = [&preferred, &alias, &marker];
  wait_for_paths(&paths, guard.child()).unwrap_or_else(|status| {
    panic!(
      "daemon exited with {status} before publishing {:?}",
      missing_paths(&paths)
    );
  });

  assert!(
    guard
      .child()
      .try_wait()
      .expect("query child status")
      .is_none(),
    "child exited before SIGTERM"
  );
  sigterm(guard.child());

  let Some(status) = wait_for_exit(guard.child()) else {
    let mut child = guard.take();
    let _ = child.kill();
    let killed = child.wait().expect("reap daemon after kill");
    panic!("daemon did not exit within {WAIT:?} of SIGTERM (reaped as {killed})");
  };
  drop(guard.take());

  assert!(
    status.success(),
    "daemon must exit cleanly on SIGTERM: {status}"
  );
  for path in [&preferred, &alias, &marker] {
    assert!(
      fs::symlink_metadata(path).is_err(),
      "graceful shutdown must remove {}",
      path.display()
    );
  }
}

/// Two daemons share a runtime dir but reserve distinct indexes; SIGTERM of
/// one removes only its own socket, alias, and marker.
#[test]
fn two_daemons_reserve_distinct_indexes_and_sigterm_removes_only_own() {
  let scratch = Scratch::new();
  let runtime = scratch.path();
  let home_a = runtime.join("home-a");
  let home_b = runtime.join("home-b");
  let cache_a = runtime.join("cache-a");
  let cache_b = runtime.join("cache-b");
  for dir in [&home_a, &home_b, &cache_a, &cache_b] {
    fs::DirBuilder::new()
      .mode(0o700)
      .create(dir)
      .expect("create private dir");
  }

  // Offset past the single-daemon SIGTERM test's pid-derived port block.
  let base = 30000 + (std::process::id() % 20000);
  let mut first = daemon(runtime, &home_a, &cache_a, base + 1000);
  let mut second = daemon(runtime, &home_b, &cache_b, base + 2000);
  let mut a = ChildGuard::new(first.spawn().expect("spawn first daemon"));
  let mut b = ChildGuard::new(second.spawn().expect("spawn second daemon"));

  let preferred = runtime.join("app/com.discordapp.Discord");
  let sockets = [
    preferred.join("discord-ipc-0"),
    preferred.join("discord-ipc-1"),
  ];
  // SAFETY: geteuid has no preconditions and cannot fail.
  let euid = unsafe { libc::geteuid() };
  let aliases = [runtime.join("discord-ipc-0"), runtime.join("discord-ipc-1")];
  let markers = [
    runtime.join(format!(".adsum-ipc-owner-{euid}-0")),
    runtime.join(format!(".adsum-ipc-owner-{euid}-1")),
  ];
  let published: [&Path; 6] = [
    &sockets[0],
    &sockets[1],
    &aliases[0],
    &aliases[1],
    &markers[0],
    &markers[1],
  ];
  wait_for_paths_two(&published, a.child(), b.child());

  for (index, socket) in sockets.iter().enumerate() {
    assert_eq!(
      fs::read_link(&aliases[index]).expect("runtime alias"),
      *socket,
      "alias {index} must point at its preferred socket"
    );
    assert!(
      fs::symlink_metadata(&markers[index])
        .expect("ownership marker")
        .is_file(),
      "marker {index} must be a regular file"
    );
  }

  sigterm(a.child());
  let Some(status) = wait_for_exit(a.child()) else {
    let mut child = a.take();
    let _ = child.kill();
    let killed = child.wait().expect("reap daemon after kill");
    panic!("first daemon did not exit within {WAIT:?} of SIGTERM (reaped as {killed})");
  };
  drop(a.take());
  assert!(
    status.success(),
    "first daemon must exit cleanly on SIGTERM: {status}"
  );

  let gone = sockets
    .iter()
    .position(|path| fs::symlink_metadata(path).is_err())
    .expect("the first daemon must release its index");
  let kept = 1 - gone;
  assert!(
    fs::symlink_metadata(&sockets[kept]).is_ok(),
    "the surviving daemon's socket must stay"
  );
  let gone_alias = runtime.join(format!("discord-ipc-{gone}"));
  let gone_marker = runtime.join(format!(".adsum-ipc-owner-{euid}-{gone}"));
  assert!(
    fs::symlink_metadata(&gone_alias).is_err(),
    "alias {gone} must be removed with its owner"
  );
  assert!(
    fs::symlink_metadata(&gone_marker).is_err(),
    "marker {gone} must be removed with its owner"
  );
  let kept_alias = runtime.join(format!("discord-ipc-{kept}"));
  let kept_marker = runtime.join(format!(".adsum-ipc-owner-{euid}-{kept}"));
  assert_eq!(
    fs::read_link(&kept_alias).expect("kept alias"),
    sockets[kept]
  );
  assert!(
    fs::symlink_metadata(&kept_marker).is_ok(),
    "marker {kept} must stay with the survivor"
  );

  assert!(
    b.child().try_wait().expect("query second daemon").is_none(),
    "the second daemon must outlive the first"
  );
  handshake(&sockets[kept]);
  handshake(&kept_alias);

  sigterm(b.child());
  let Some(status) = wait_for_exit(b.child()) else {
    let mut child = b.take();
    let _ = child.kill();
    let killed = child.wait().expect("reap daemon after kill");
    panic!("second daemon did not exit within {WAIT:?} of SIGTERM (reaped as {killed})");
  };
  drop(b.take());
  assert!(
    status.success(),
    "second daemon must exit cleanly on SIGTERM: {status}"
  );

  for (index, socket) in sockets.iter().enumerate() {
    for path in [
      socket.clone(),
      runtime.join(format!("discord-ipc-{index}")),
      runtime.join(format!(".adsum-ipc-owner-{euid}-{index}")),
    ] {
      assert!(
        fs::symlink_metadata(&path).is_err(),
        "graceful shutdown must remove {}",
        path.display()
      );
    }
  }
}
