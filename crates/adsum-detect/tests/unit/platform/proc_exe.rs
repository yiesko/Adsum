//! Integration smoke tests for `/proc` reads against a real subprocess.

use crate::platform::linux::proc_readers::{read_exec_into, read_image_path_into};
use crate::types::Exec;
use std::process::Command;
use std::time::Duration;

#[test]
fn read_exec_into_captures_sleep() {
  let mut child = Command::new("sleep")
    .arg("30")
    .spawn()
    .expect("spawn sleep");
  let pid = child.id() as u64;
  // Give the kernel time to publish /proc.
  std::thread::sleep(Duration::from_millis(50));
  let mut exec = Exec::default();
  let mut scratch = crate::scratch::ExecScratch::default();
  let ok = read_exec_into(pid, &mut exec, &mut scratch);
  let _ = child.kill();
  let _ = child.wait();
  if !ok {
    // Permission/namespace restricted: skip.
    return;
  }
  assert!(exec.path.ends_with("sleep"));
  let mut buf = String::new();
  let image = read_image_path_into(pid, &mut buf);
  // Process may have exited; accept None.
  if let Some(path) = image {
    assert!(path.ends_with("sleep"));
  }
}

#[test]
fn read_cwd_into_captures_working_dir() {
  use crate::platform::read_cwd_into;
  use std::env::temp_dir;
  let dir = temp_dir().join("adsum-cwd-test");
  let _ = std::fs::create_dir_all(&dir);
  let mut child = Command::new("sleep")
    .arg("30")
    .current_dir(&dir)
    .spawn()
    .expect("spawn sleep");
  let pid = child.id() as u64;
  std::thread::sleep(Duration::from_millis(50));
  let mut cwd_buf = Vec::new();
  let mut path_buf = String::new();
  let res = read_cwd_into(pid, &mut cwd_buf, &mut path_buf);
  let _ = child.kill();
  let _ = child.wait();
  if res.is_none() {
    return;
  }
  let cwd = String::from_utf8_lossy(&cwd_buf);
  assert!(cwd.contains("adsum-cwd-test"));
}
