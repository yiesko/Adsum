//! The `sysinfo` snapshot backend (every non-Linux target).
//!
//! Named for the mechanism, not `sysinfo`, so in-module paths never
//! shadow the external crate. Two phases, same contract as `/proc`:
//! `begin_tick` refreshes the shared `System` snapshot (exe and cmd,
//! `OnlyIfNotSet` — sysinfo caches them per process) and collects the
//! pid list; `read_one` copies one process out of that snapshot into the
//! caller's reused slot. The snapshot cannot change between the two
//! calls (only this thread refreshes the handle), so the phase split
//! costs nothing semantically — it is what lets the shared orchestration
//! (full sweep AND incremental mode) run unchanged on these targets.
//!
//! Per-pid cost is one uncontended mutex acquire plus two buffer
//! refills; the old shape rebuilt the whole `Vec<Exec>` every tick
//! (~3 allocations per process). If the per-pid lock ever shows up in a
//! profile, the fix is a snapshot generation counter that lets
//! `read_one` skip re-locking within a tick — not worth the state today.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, UpdateKind};

use adsum_protocol::error::Result;

use crate::scratch::{EnumerateScratch, ExecScratch};
use crate::server::ProcessServer;
use crate::traits::ProcessEnumerator;
use crate::types::Exec;

/// The non-Linux backend: one shared `sysinfo::System` snapshot per tick.
pub(crate) struct SnapshotEnumerator;

impl ProcessEnumerator for SnapshotEnumerator {
  fn begin_tick(server: &ProcessServer, scratch: &mut EnumerateScratch) -> Result<()> {
    let mut sys = server.sysinfo.lock().unwrap_or_else(|e| e.into_inner());
    sys.refresh_processes_specifics(
      ProcessesToUpdate::All,
      true,
      ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::OnlyIfNotSet)
        .with_cmd(UpdateKind::OnlyIfNotSet),
    );
    scratch.pids.clear();
    scratch
      .pids
      .extend(sys.processes().keys().map(|pid| u64::from(pid.as_u32())));
    Ok(())
  }

  fn read_one(
    server: &ProcessServer,
    pid: u64,
    slot: &mut Exec,
    _scratch: &mut ExecScratch,
  ) -> bool {
    let Ok(pid32) = u32::try_from(pid) else {
      return false;
    };
    let sys = server.sysinfo.lock().unwrap_or_else(|e| e.into_inner());
    let Some(proc) = sys.process(Pid::from_u32(pid32)) else {
      // Cannot happen inside a tick (nothing else prunes the snapshot),
      // but the contract says: unreadable is a skip, never a hole.
      return false;
    };
    slot.pid = pid;
    // Path buffer reused: `to_string_lossy` borrows for valid UTF-8 (the
    // common case), so the copy lands in the slot's own String.
    // No exe resolves to the empty path — the historical
    // `exe().unwrap_or(Path::new(""))` shape, minus its allocation.
    slot.path.clear();
    if let Some(exe) = proc.exe() {
      slot.path.push_str(&exe.to_string_lossy());
    }
    // Arguments buffer reused: take the slot's own String, refill it, put
    // it back. Semantics identical to the historical per-tick rebuild:
    // `Some(space-joined argv[1..])` whenever argv is non-empty
    // (`Some("")` for a lone argv[0]), `None` otherwise.
    let mut args = slot.arguments.take().unwrap_or_default();
    args.clear();
    let mut cmd = proc.cmd().iter();
    if cmd.next().is_some() {
      let mut first = true;
      for arg in cmd {
        if !first {
          args.push(' ');
        }
        first = false;
        args.push_str(&arg.to_string_lossy());
      }
      slot.arguments = Some(args);
    }
    true
  }
}
