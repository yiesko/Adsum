//! Scan cadence: the idle backoff and the watcher-live gate around it.

use std::time::Duration;

/// Idle-stretched cadence: base × 2^idle_ticks, capped at 30s
/// (5s → 10s → 20s → 30s at the default base). Overflow-safe.
pub fn idle_wait(base: Duration, idle_ticks: u32) -> Duration {
  const MAX_BACKOFF: Duration = Duration::from_secs(30);
  let stretched = base
    .checked_mul(1 << idle_ticks.min(4))
    .unwrap_or(MAX_BACKOFF);
  stretched.min(MAX_BACKOFF)
}

/// Effective scan cadence: idle backoff applies only while the EXEC
/// watcher is confirmed live (socket up). Polling-only paths — other
/// OSes, disabled proc-events, failed watcher setups and retry windows —
/// always use the configured base interval, or a short interval would
/// silently grow to 30s with nothing to wake the scan early.
pub(crate) fn scan_cadence(base: Duration, idle_ticks: u32, watcher_live: bool) -> Duration {
  if watcher_live {
    idle_wait(base, idle_ticks)
  } else {
    base
  }
}
