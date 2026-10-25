//! The `/proc` enumerator: raw `getdents64` batches into a reused,
//! 8-byte-aligned arena, each record's `d_name` parsed in place — zero
//! allocations per tick once warm (see `enumerate_pids_into` for why
//! this replaced `std::fs::read_dir`).

use adsum_protocol::error::Result;

use crate::scratch::{EnumerateScratch, ExecScratch};
use crate::server::ProcessServer;
use crate::traits::ProcessEnumerator;
use crate::types::Exec;

/// Parse a `/proc` entry name into its pid: ASCII digits only, so a
/// non-UTF-8 or non-numeric name never panics and never allocates.
///
/// Takes the raw `d_name` bytes the `getdents64` sweep hands back (see
/// [`enumerate_pids_into`]): parsing in place avoids the owned `OsString`
/// `DirEntry::file_name()` allocated per entry, on a path that runs ~1k
/// times per tick. Anything non-numeric is skipped, never fatal.
///
/// Only the `/proc` sweep calls it, so it is dead weight on targets that
/// list processes through `sysinfo`.
pub(crate) fn parse_proc_pid(name: &[u8]) -> Option<u64> {
  if name.is_empty() {
    return None;
  }
  let mut pid: u64 = 0;
  for &byte in name {
    if !byte.is_ascii_digit() {
      return None;
    }
    pid = pid.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
  }
  Some(pid)
}

/// `getdents64` arena size in `u64` cells (32 KiB): comfortably holds
/// hundreds of `/proc` entries per batch, and a busier host simply pays
/// one more syscall per batch, never a realloc.
const DENT_ARENA_U64S: usize = 32 * 1024 / 8;

/// Enumerate the live pids of `/proc` into `scratch.pids`, allocation-free
/// once warm: raw `getdents64` batches land in the reused, 8-byte-aligned
/// `scratch.dents` arena and each record's `d_name` is parsed in place.
///
/// Replaces `std::fs::read_dir`, whose iterator allocates per entry
/// (boxed dirent) and whose `DirEntry::file_name()` hands back an owned
/// `OsString` — measured ~3 allocations per `/proc` entry per tick on the
/// production shape (the old code even called `file_name()` twice per pid:
/// once in the filter, once in the loop). Same order (both are the raw
/// directory order), same skip rules ([`parse_proc_pid`]), same error
/// contract (`Err` only when `/proc` itself is unreadable).
pub(crate) fn enumerate_pids_into(scratch: &mut EnumerateScratch) -> std::io::Result<()> {
  use std::os::unix::io::AsRawFd;

  // Record field offsets: `offset_of!` over libc's `dirent64` keeps the
  // parse layout-correct on every arch (x86-64, armv7, aarch64, ...) —
  // the header is 19 bytes in practice, but the type says so, not us.
  const RECLEN_OFF: usize = std::mem::offset_of!(libc::dirent64, d_reclen);
  const NAME_OFF: usize = std::mem::offset_of!(libc::dirent64, d_name);

  scratch.pids.clear();
  if scratch.dents.is_empty() {
    scratch.dents.resize(DENT_ARENA_U64S, 0);
  }
  let dir = std::fs::File::open("/proc")?;
  let fd = dir.as_raw_fd();
  let buf = scratch.dents.as_mut_ptr().cast::<u8>();
  let buf_len = scratch.dents.len() * size_of::<u64>();
  loop {
    // SAFETY: getdents64 writes whole records into [buf, buf+buf_len);
    // the arena is live, 8-byte aligned (Vec<u64>) and at least one
    // record wide, and `fd` is an open directory descriptor.
    let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf, buf_len) };
    if n < 0 {
      return Err(std::io::Error::last_os_error());
    }
    if n == 0 {
      break;
    }
    let n = n as usize;
    let mut off = 0usize;
    while off + NAME_OFF < n {
      // SAFETY: the record header lies within the bytes just written;
      // `read_unaligned` makes no alignment assumption, and `from_raw_
      // parts` over u8 stays inside the record's own `d_reclen` bound.
      let reclen = usize::from(unsafe { buf.add(off + RECLEN_OFF).cast::<u16>().read_unaligned() });
      if reclen <= NAME_OFF || off + reclen > n {
        // Torn record (cannot happen with a well-behaved kernel): stop
        // this batch rather than walk off it. The pid simply waits for
        // the next tick's sweep — the scan is the backstop by design.
        break;
      }
      let name = unsafe { std::slice::from_raw_parts(buf.add(off + NAME_OFF), reclen - NAME_OFF) };
      let name = match name.iter().position(|&byte| byte == 0) {
        Some(nul) => &name[..nul],
        None => name,
      };
      if let Some(pid) = parse_proc_pid(name) {
        scratch.pids.push(pid);
      }
      off += reclen;
    }
  }
  Ok(())
}

/// The Linux backend: `/proc` for both phases — `getdents64` for
/// liveness, `read_exec_into` for the slot reads.
pub(crate) struct ProcfsEnumerator;

impl ProcessEnumerator for ProcfsEnumerator {
  fn begin_tick(_server: &ProcessServer, scratch: &mut EnumerateScratch) -> Result<()> {
    enumerate_pids_into(scratch)?;
    Ok(())
  }

  fn read_one(
    _server: &ProcessServer,
    pid: u64,
    slot: &mut Exec,
    scratch: &mut ExecScratch,
  ) -> bool {
    super::proc_readers::read_exec_into(pid, slot, scratch)
  }
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/enumerator.rs"]
mod tests;
