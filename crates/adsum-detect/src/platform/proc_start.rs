//! Process start times for detection-latency attribution — one public
//! function, two backends behind it (the unified shape the `platform`
//! module exists for): Linux parses `/proc/<pid>/stat` directly (no new
//! dependency); other targets use a call-scoped `sysinfo` refresh.
//!
//! The scan tick never pays for this: the start time is read once per
//! newly detected game (a rare event), never per process per tick. The
//! public path stays `adsum_detect::proc_start` (re-exported from
//! `lib.rs`), so embedders never see the move.

/// Start time of `pid` as epoch millis (`None` for vanished pids and
/// unreadable tables).
#[cfg(target_os = "linux")]
pub fn process_start_ms(pid: u64) -> Option<u64> {
  use std::sync::OnceLock;

  static BOOT_MS: OnceLock<Option<u64>> = OnceLock::new();
  static CLK_TCK: OnceLock<u64> = OnceLock::new();

  let boot_ms = (*BOOT_MS.get_or_init(|| {
    std::fs::read_to_string("/proc/stat").ok().and_then(|stat| {
      stat
        .lines()
        .find_map(|line| {
          line
            .strip_prefix("btime ")
            .and_then(|secs| secs.trim().parse::<u64>().ok())
        })
        .map(|secs| secs.saturating_mul(1000))
    })
  }))?;
  // USER_HZ is 100 on every Linux procfs; sysconf only runs once.
  let clk_tck = *CLK_TCK.get_or_init(|| {
    // SAFETY: sysconf with a valid name is always safe to call.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks).unwrap_or(100).max(1)
  });
  let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
  let ticks = stat_starttime_ticks(&stat)?;
  Some(start_ms_from_ticks(boot_ms, ticks, clk_tck))
}

/// Start time of `pid` as epoch millis via a single-process refresh
/// (rare event only — never on the scan tick).
///
/// `nothing()` is enough: `sysinfo` fills `start_time` unconditionally
/// when it lists a process (it is not behind an `UpdateKind` field), so
/// the old `everything()` was paying for cwd, environ, user and root
/// reads per detection to throw all of them away.
#[cfg(not(target_os = "linux"))]
pub fn process_start_ms(pid: u64) -> Option<u64> {
  use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

  let mut sys = System::new();
  let pid = Pid::from_u32(u32::try_from(pid).ok()?);
  sys.refresh_processes_specifics(
    ProcessesToUpdate::Some(&[pid]),
    true,
    ProcessRefreshKind::nothing(),
  );
  sys
    .process(pid)
    .map(|process| process.start_time().saturating_mul(1000))
}

/// `starttime` (field 22, clock ticks since boot) from a `/proc` stat
/// line. The comm may hold spaces and parens, so fields start after
/// the LAST `)` — everything past it is numeric.
#[cfg(target_os = "linux")]
fn stat_starttime_ticks(stat: &str) -> Option<u64> {
  let close = stat.rfind(')')?;
  if !stat[..close].contains('(') {
    return None;
  }
  stat[close + 1..].split_whitespace().nth(19)?.parse().ok()
}

/// Absolute start millis from boot millis, start ticks and clock ticks.
#[cfg(target_os = "linux")]
fn start_ms_from_ticks(boot_ms: u64, ticks: u64, clk_tck: u64) -> u64 {
  boot_ms.saturating_add(ticks.saturating_mul(1000) / clk_tck.max(1))
}

#[cfg(test)]
#[path = "../../tests/unit/proc_start.rs"]
mod tests;
