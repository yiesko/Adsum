//! Linux `/proc` readers: the lazy per-pid reads the shared match
//! pipeline calls through `crate::platform`'s uniform surface. Every
//! reader takes the caller's reuse buffers — a miss costs no allocation
//! of its own — and every size cap lives here (64 KiB cmdline, 256 KiB
//! environ, 8 KiB stat, PATH_MAX cwd), so adversarial inputs cannot
//! multiply per process per tick.

use crate::scratch::ExecScratch;
use crate::types::Exec;

/// Read one process's cmdline into an owned `Exec`: the throwaway twin
/// of [`read_exec_into`], kept for the parity test (and any future
/// one-shot caller) — production paths always refill a reused slot.
/// `None` for kernel threads, zombies, vanished or unreadable pids.
///
/// Bounded: at most 64 KiB are read (`argv[0]` lives at the start, so
/// truncation only ever cuts late arguments, never the path). Adversarial
/// megabyte-cmdlines would otherwise multiply per process per tick.
#[cfg(test)]
pub(crate) fn read_exec(pid: u64) -> Option<Exec> {
  let mut exec = Exec::default();
  let mut scratch = ExecScratch::default();
  read_exec_into(pid, &mut exec, &mut scratch).then_some(exec)
}

// Per-pid allocation reuse for the scan tick: `read_exec` intermediates
// live in caller-owned `ExecScratch` buffers (one set per thread) and
// output strings reuse the destination `Exec` slot's capacity.
pub fn read_exec_into(pid: u64, exec: &mut Exec, scratch: &mut ExecScratch) -> bool {
  const MAX_CMDLINE_BYTES: u64 = 64 * 1024;
  use std::fmt::Write as _;
  scratch.path.clear();
  // `write!` into a cleared buffer reuses it; the only failure mode is
  // OOM (fmt::Error), matching the old `format!` behavior if allocation
  // fails.
  if write!(scratch.path, "/proc/{pid}/cmdline").is_err() {
    return false;
  }
  // NOTE: no metadata size check here — /proc files report st_size 0
  // despite having content; an early `len() == 0` return would skip
  // EVERY process (total detection blindness).
  let Ok(file) = std::fs::File::open(&scratch.path) else {
    return false;
  };
  scratch.cmdline.clear();
  use std::io::Read;
  if file
    .take(MAX_CMDLINE_BYTES + 1)
    .read_to_end(&mut scratch.cmdline)
    .is_err()
  {
    return false;
  }
  if scratch.cmdline.is_empty() {
    return false;
  }
  scratch
    .cmdline
    .truncate(usize::try_from(MAX_CMDLINE_BYTES).unwrap_or(usize::MAX));
  // Truncation may split a multibyte char: back off to the boundary
  // in one step (never rescan: `valid_up_to` is the split point).
  if let Err(err) = std::str::from_utf8(&scratch.cmdline) {
    scratch.cmdline.truncate(err.valid_up_to());
  }
  let Ok(text) = std::str::from_utf8(&scratch.cmdline) else {
    return false;
  };
  let mut cmd_iter = text.split('\0');
  let cmd_path = cmd_iter.next().unwrap_or("");
  // Output strings reuse the destination slot's capacity: `path` is
  // cleared and refilled in place; the old args buffer (if any) is
  // returned to the scratch for the next pid, so the two buffers trade
  // places instead of reallocating.
  exec.pid = pid;
  exec.path.clear();
  exec.path.push_str(cmd_path);
  scratch.args.clear();
  for (index, arg) in cmd_iter.enumerate() {
    if index > 0 {
      scratch.args.push(' ');
    }
    scratch.args.push_str(arg);
  }
  let previous = exec.arguments.take();
  if !scratch.args.is_empty() {
    exec.arguments = Some(std::mem::take(&mut scratch.args));
  }
  if let Some(mut old) = previous {
    old.clear();
    scratch.args = old;
  }
  true
}

/// Read `SteamAppId` from `/proc/<pid>/environ` (set by Steam/Proton for
/// every game process it launches, including non-Steam shortcuts).
/// Callers read this lazily: environ is the most expensive read of the
/// scan, so only misses pay for it.
///
/// The scratch supplies both buffers — `path` for the `/proc/<pid>` prefix
/// and `environ` for the body — so a miss (every non-Steam process) costs
/// no allocation of its own, and a hit keeps the buffer sized for the
/// next one.
pub(crate) fn read_steam_app_id(pid: u64, scratch: &mut ExecScratch) -> Option<String> {
  // Bounded: 256 KiB covers any legitimate environ (SteamAppId lives
  // near the front); larger means hostile or broken, and reading it
  // whole per new pid would multiply per tick.
  const MAX_ENVIRON_BYTES: u64 = 256 * 1024;

  // NOTE: no metadata size check — /proc files report st_size 0 (see above).
  use std::fmt::Write as _;
  use std::io::Read;

  // Same buffer the exec reader reuses: cleared and refilled per pid.
  scratch.path.clear();
  write!(scratch.path, "/proc/{pid}/environ").ok()?;
  let file = std::fs::File::open(&scratch.path).ok()?;

  scratch.environ.clear();
  file
    .take(MAX_ENVIRON_BYTES + 1)
    .read_to_end(&mut scratch.environ)
    .ok()?;
  scratch
    .environ
    .truncate(usize::try_from(MAX_ENVIRON_BYTES).unwrap_or(usize::MAX));
  for entry in scratch.environ.split(|b| *b == 0) {
    if let Some(id) = entry.strip_prefix(b"SteamAppId=")
      && let Ok(id) = std::str::from_utf8(id)
    {
      let id = id.trim();
      if !id.is_empty() {
        return Some(id.to_string());
      }
    }
  }
  None
}

/// Process working directory via `/proc/<pid>/cwd` (one readlink, no file
/// content), read into the caller's reuse buffers: `buf` receives the
/// link target (lowercased in place — ASCII fold, byte-identical to the
/// old `to_ascii_lowercase`), `path` carries the `/proc/<pid>/cwd`
/// spelling. Proton launches with the game dir as cwd, so joining it with
/// a bare exe reconstructs the install path the DB knows (DOOM Eternal
/// case). Read lazily: only bare-exe misses pay for it.
///
/// Zero allocations once warm: the previous shape paid three per miss
/// (`format!` path, `read_link`'s `PathBuf`, the lowercase `String`).
/// `None` for unreadable links, non-UTF-8 targets (same rule as the old
/// `to_str()?`) and oversized ones (past `MAX_CWD_BYTES`: hostile or
/// broken, and no real cwd is kilobytes deep).
pub(crate) fn read_cwd_into(pid: u64, buf: &mut Vec<u8>, path: &mut String) -> Option<()> {
  use std::fmt::Write as _;
  // PATH_MAX on Linux is 4096; readlink truncates to the buffer, and a
  // truncated cwd would join into a wrong path — refuse it instead.
  const MAX_CWD_BYTES: usize = 4096;

  path.clear();
  // Trailing NUL rides inside the String: readlink wants a C string, and
  // `write!` into the cleared buffer reuses it (OOM is the only failure).
  write!(path, "/proc/{pid}/cwd\0").ok()?;
  if buf.capacity() < MAX_CWD_BYTES {
    buf.reserve(MAX_CWD_BYTES - buf.capacity());
  }
  // SAFETY: `path` is a NUL-terminated C string in a live buffer; `buf`
  // has at least MAX_CWD_BYTES writable bytes behind its pointer; the
  // kernel writes at most that many and returns the count (or -1).
  let n = unsafe { libc::readlink(path.as_ptr().cast(), buf.as_mut_ptr().cast(), MAX_CWD_BYTES) };
  if n < 0 || usize::try_from(n).unwrap_or(usize::MAX) >= MAX_CWD_BYTES {
    return None;
  }
  // SAFETY: readlink wrote exactly `n` bytes into the spare capacity.
  unsafe { buf.set_len(n as usize) };
  // Same UTF-8 rule as the old `to_str()?`, then fold ASCII in place.
  std::str::from_utf8(buf).ok()?;
  for byte in buf.iter_mut() {
    byte.make_ascii_lowercase();
  }
  Some(())
}

/// Whether a process is currently suspended (`T` state: SIGSTOP'd, e.g. a
/// paused game). Suspended games show a frozen frame (or nothing) — they
/// are not being played, so matches on them are discarded and the scan
/// treats them as absent (clears). Checked lazily, only for matched
/// processes: one tiny `stat` read per hit, never per scan.
///
/// `stat` is the caller's reuse buffer (see [`MatchScratch::stat`]) and
/// does double duty — it carries the `/proc/<pid>/stat` path first, then
/// the body — the same cleared-and-refilled contract `ExecScratch::path`
/// already has, so a hit costs no allocation of its own (the path used
/// to be a fresh `format!` String per hit). Cleared before every read so
/// a failed one can never leave a stale line to parse.
pub fn is_suspended(pid: u64, stat: &mut String) -> bool {
  // Capped like every other `/proc` read (`stat` is tiny, but the cap
  // documents the rule has no exceptions).
  use std::fmt::Write as _;
  use std::io::Read;
  stat.clear();
  // `write!` into a cleared buffer reuses it; the only failure mode is
  // OOM (fmt::Error), matching the old `format!` behavior if allocation
  // fails. Every failure path clears the buffer before returning, so a
  // failed check can never leave bytes behind for a later parse to
  // misread (the invariant the reuse test pins).
  if write!(stat, "/proc/{pid}/stat").is_err() {
    stat.clear();
    return false;
  }
  let Ok(file) = std::fs::File::open(&*stat) else {
    stat.clear();
    return false;
  };
  stat.clear();
  file
    .take(8 * 1024)
    .read_to_string(stat)
    .ok()
    .and_then(|_| parse_stat_state(stat))
    .is_some_and(|state| state == 'T' || state == 't')
}

/// Process state from `/proc/<pid>/stat`: the field right after the last
/// `)` (comm may itself contain spaces and parens). `None` when unreadable
/// or malformed — never counted as suspended. Linux-only like its sole
/// caller: without `/proc` there is nothing to parse.
#[inline]
pub fn parse_stat_state(stat: &str) -> Option<char> {
  stat
    .rfind(')')
    .and_then(|end| stat[end + 1..].split_whitespace().next())
    .and_then(|state| state.chars().next())
}
