//! Shared integration-test helpers: scratch dirs, frame I/O, sink drain.
//!
//! Import with `#[path = "common/mod.rs"] mod common;` — integration test
//! files are separate crates and cannot otherwise share code. Each target
//! uses a subset, hence the blanket allow below.
//!
//! ```rust,ignore
//! #[path = "common/mod.rs"]
//! mod common;
//! ```
#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use adsum_transport_ipc::frame::{PacketType, encode};
use adsum_types::cmd::ActivityCmd;

/// Unique scratch dir, removed on drop.
pub struct Scratch {
  path: PathBuf,
}

impl Scratch {
  /// Fresh unique temp dir for one test (pre-cleaned).
  pub fn new(tag: &str) -> Self {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // macOS caps `sun_path` at 104 bytes; `/tmp` keeps the full Flatpak
    // alias layout short enough for the socket tests there.
    let root = if cfg!(target_os = "macos") {
      PathBuf::from("/tmp")
    } else {
      std::env::temp_dir()
    };
    // Canonicalize so platform symlinks (macOS `/var`) do not fail the
    // private-directory gate for tests.
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let path = root.join(format!(
      "adsum-ipc-test-{}-{}-{}",
      std::process::id(),
      tag,
      COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    // Remove leftovers from a crashed run first.
    let _ = std::fs::remove_dir_all(&path);
    std::fs::DirBuilder::new()
      .mode(0o700)
      .create(&path)
      .expect("scratch dir");
    Self { path }
  }

  /// Created subdirectory inside the scratch root.
  pub fn sub(&self, name: &str) -> PathBuf {
    let dir = self.path.join(name);
    std::fs::DirBuilder::new()
      .recursive(true)
      .mode(0o700)
      .create(&dir)
      .expect("scratch subdir");
    dir
  }

  /// Scratch root path.
  pub fn path(&self) -> &Path {
    &self.path
  }
}

impl Drop for Scratch {
  /// Remove the scratch dir (best-effort, panic-safe).
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

/// Write one IPC frame.
pub fn write_frame(stream: &mut UnixStream, packet: PacketType, body: &str) {
  stream
    .write_all(&encode(packet, body))
    .expect("write frame");
}

/// Read one IPC frame: `(packet_type, body)`.
pub fn read_frame(stream: &mut UnixStream) -> (u32, String) {
  let mut header = [0_u8; 8];
  stream.read_exact(&mut header).expect("read header");
  let packet_type = u32::from_le_bytes(header[0..4].try_into().unwrap());
  let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
  let mut body = vec![0_u8; len];
  stream.read_exact(&mut body).expect("read body");
  (packet_type, String::from_utf8(body).expect("utf8 body"))
}

/// Drain one sink event, failing (not hanging) after 5s.
pub fn recv_cmd(rx: &mut tokio::sync::mpsc::Receiver<ActivityCmd>) -> ActivityCmd {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    match rx.try_recv() {
      Ok(cmd) => return cmd,
      Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
        assert!(
          Instant::now() < deadline,
          "timed out waiting for sink event"
        );
        std::thread::sleep(Duration::from_millis(5));
      }
      Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
        panic!("sink closed unexpectedly")
      }
    }
  }
}
