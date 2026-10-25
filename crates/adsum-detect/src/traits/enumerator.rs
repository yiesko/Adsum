//! The process-enumeration seam: how one target turns "the OS process
//! table" into pids and `Exec` slots.
//!
//! Static-method trait, not dyn: exactly one implementation compiles per
//! target (`crate::platform::PlatformEnumerator`), so every call is a
//! direct one — the seam costs nothing at runtime and the shared tick
//! orchestration in `sweep.rs` never branches on the OS.
//!
//! The two-phase shape (enumerate liveness, then read slots) is what
//! lets the incremental scan mode stay platform-neutral: a tick always
//! gets the fresh pid list first (the liveness truth), and only then
//! decides which pids actually need a slot read.

use adsum_protocol::error::Result;

use crate::scratch::{EnumerateScratch, ExecScratch};
use crate::server::ProcessServer;
use crate::types::Exec;

/// One target's process-table backend.
pub(crate) trait ProcessEnumerator {
  /// One liveness pass: refresh the underlying source when it is
  /// snapshot-based, then fill `scratch.pids` with every visible pid in
  /// enumeration order (the order the detected table reports in). Must
  /// not allocate once the scratch is warm.
  fn begin_tick(server: &ProcessServer, scratch: &mut EnumerateScratch) -> Result<()>;

  /// Fill one slot for `pid` (cmdline on Linux, snapshot copy elsewhere),
  /// reusing the slot's buffers. `false` means unreadable — vanished,
  /// zombie, kernel thread, permission: the caller skips it without
  /// leaving a hole, exactly like the historical `/proc` sweep.
  fn read_one(server: &ProcessServer, pid: u64, slot: &mut Exec, scratch: &mut ExecScratch)
  -> bool;
}
