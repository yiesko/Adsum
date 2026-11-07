//! Snapshot-backend readers (every non-Linux target).
//!
//! `read_steam_app_id` is real here, not a stub: sysinfo 0.39 reads the
//! environ block on Windows (PEB through the process handle) and macOS
//! (the sysctl arg/env route), so the SteamAppId tier — the
//! authoritative answer for the majority of database entries, which
//! declare no executables — works off Linux too, memoized per pid by the
//! shared `AppIdMemo` exactly like the `/proc` reader. The other two are
//! honest no-ops: no `readlink`-shaped cwd source and no reliable
//! suspended-state read exist through sysinfo on both targets (a
//! `Process::status()` mapping is a possible future upgrade on Windows).

use crate::scratch::ExecScratch;

/// SteamAppId via the `sysinfo` environ block (Windows and macOS): the
/// same contract the Linux `/proc/<pid>/environ` reader has — kilobytes,
/// read once per process lifetime because the AppId memo above caches
/// `Present`/`Absent` — and the same fail-open shape: both platforms
/// gate other users' (or elevated/protected) processes, an unreadable
/// environ reports `None` and memoizes `Absent` exactly like a Linux
/// permission denial, so the tier simply stays dark there while the
/// automaton and command-line tiers carry on.
///
/// Windows reads the PEB environment block through the process handle,
/// macOS the sysctl arg/env route (`sysinfo` 0.39 implements both; the
/// scanner previously stubbed this to `None`, leaving 57% of the
/// database — the entries with no declared executables — without its
/// authoritative tier off Linux).
///
/// The `System` is scoped to this call and dropped on return: the
/// server's shared snapshot never accumulates one environ copy per pid
/// for the whole session, and the per-read cost is paid once per process
/// lifetime by the memo above. The scratch parameter keeps the Linux
/// signature (its buffers serve `/proc` reads only).
pub(crate) fn read_steam_app_id(pid: u64, _scratch: &mut ExecScratch) -> Option<String> {
  use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

  let pid = Pid::from_u32(u32::try_from(pid).ok()?);
  let mut sys = System::new();
  sys.refresh_processes_specifics(
    ProcessesToUpdate::Some(&[pid]),
    true,
    ProcessRefreshKind::nothing().with_environ(UpdateKind::OnlyIfNotSet),
  );
  sys.process(pid)?.environ().iter().find_map(|var| {
    // The key and the value are ASCII by contract; a non-UTF-8 environ
    // entry cannot carry a SteamAppId, so `to_str()` skips it without a
    // lossy copy.
    let (name, value) = var.to_str()?.split_once('=')?;
    if name != "SteamAppId" {
      return None;
    }
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
  })
}

/// Read the process working directory. macOS supports this through
/// `proc_pidinfo`; other non-Linux targets remain unsupported.
#[allow(clippy::ptr_arg)]
pub(crate) fn read_cwd_into(_pid: u64, _buf: &mut Vec<u8>, _path: &mut String) -> Option<()> {
  #[cfg(target_os = "macos")]
  return read_cwd_into_macos(_pid, _buf);
  #[cfg(not(target_os = "macos"))]
  None
}

/// macOS cwd reader: copy the `proc_vnodepathinfo` cwd bytes into `buf`.
#[cfg(target_os = "macos")]
fn read_cwd_into_macos(pid: u64, buf: &mut Vec<u8>) -> Option<()> {
  let pid = i32::try_from(pid).ok()?;
  let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
  // SAFETY: `info` owns writable storage for the fixed-size structure
  // the macOS kernel writes; the request size is the struct size.
  let bytes = unsafe {
    libc::proc_pidinfo(
      pid,
      libc::PROC_PIDVNODEPATHINFO,
      0,
      info.as_mut_ptr().cast::<libc::c_void>(),
      std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int,
    )
  };
  if bytes != std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int {
    return None;
  }
  // SAFETY: proc_pidinfo wrote a valid proc_vnodepathinfo.
  let info = unsafe { info.assume_init() };
  let raw = info.pvi_cdir.vip_path;
  let bytes = unsafe {
    std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), raw.len() * raw.first()?.len())
  };
  let end = bytes.iter().position(|byte| *byte == 0)?;
  buf.clear();
  buf.extend_from_slice(&bytes[..end]);
  std::str::from_utf8(buf).ok()?;
  Some(())
}

/// Stubbed on non-Linux.
pub(crate) fn read_image_path_into<'a>(
  _pid: u64,
  _buf: &'a mut String,
  _path: &'a mut String,
  _raw: &'a mut Vec<u8>,
) -> Option<&'a str> {
  None
}

/// Stubbed on non-Linux (Steam matching there is already limited).
pub(crate) fn is_suspended(_pid: u64, _stat: &mut String) -> bool {
  false
}
