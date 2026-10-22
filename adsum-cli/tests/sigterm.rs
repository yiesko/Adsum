//! End-to-end SIGTERM check: the real binary runs the normal graceful
//! shutdown and removes its socket, scratch alias, and ownership marker.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::DirBuilderExt;
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

/// SIGTERM must resolve the same shutdown as Ctrl+C: a clean exit with the
/// bound socket, scratch alias, and marker all removed.
#[test]
fn sigterm_removes_owned_socket_alias_and_marker() {
  let scratch = Scratch::new();
  let runtime = scratch.path();

  // One high, pid-derived port block; no two servers share a port.
  let base = 30000 + (std::process::id() % 20000);
  let port = |offset: u32| {
    u16::try_from(base + offset)
      .expect("pid-derived test port fits in u16")
      .to_string()
  };

  let preferred = runtime.join("app/com.discordapp.Discord/discord-ipc-0");
  let alias = runtime.join("discord-ipc-0");
  // SAFETY: geteuid has no preconditions and cannot fail.
  let euid = unsafe { libc::geteuid() };
  let marker = runtime.join(format!(".adsum-ipc-owner-{euid}-0"));

  let mut command = Command::new(env!("CARGO_BIN_EXE_adsum"));
  command
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
    .env("HOME", runtime)
    .env("XDG_CONFIG_HOME", runtime.join("config"))
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
  let pid = libc::pid_t::try_from(guard.child().id()).expect("child pid fits in pid_t");
  // SAFETY: unreaped means still ours: the kernel cannot recycle a PID
  // until the child is waited on, so this signal cannot hit another process.
  let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
  assert_eq!(
    rc,
    0,
    "kill(SIGTERM) failed: {}",
    std::io::Error::last_os_error()
  );

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
