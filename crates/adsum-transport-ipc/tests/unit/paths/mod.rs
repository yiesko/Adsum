use std::ffi::CString;
use std::fs::Permissions;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::sync::Mutex;

use super::*;

mod aliases;
mod namespaces;
mod probes;
mod security;
mod selection;

/// Serializes the two tests that use the uid-qualified `/tmp` fallback.
static FALLBACK_LOCK: Mutex<()> = Mutex::new(());

/// Isolated 0700 temp dir per test (tag-suffixed, hermetic). The temp base
/// is canonicalized so platform symlinks (macOS `/var`) keep the chain real.
fn scratch(tag: &str) -> PathBuf {
  let base = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
  let dir = base.join(format!("adsum-paths-test-{}-{tag}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::DirBuilder::new()
    .mode(0o700)
    .create(&dir)
    .expect("scratch dir");
  dir
}

/// Synthetic chain facts for owner and mode policy tests.
fn facts(uid: u32, mode: u32, mnt_id: Option<u64>) -> Facts {
  Facts { uid, mode, mnt_id }
}
