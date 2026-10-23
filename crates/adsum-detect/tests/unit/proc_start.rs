// Unit tests cover the Linux parser; other targets exercise the
// public reader through the integration test instead.
#[cfg(target_os = "linux")]
use super::*;

/// `starttime` is field 22 even when the comm holds spaces and parens
/// (fields start after the LAST `)`).
#[cfg(target_os = "linux")]
#[test]
fn stat_starttime_survives_tricky_comm() {
  let stat = "12345 (my (tricky) game) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 42 999";
  assert_eq!(stat_starttime_ticks(stat), Some(42));
}

/// Short lines and comm-less garbage refuse instead of misparsing.
#[cfg(target_os = "linux")]
#[test]
fn stat_starttime_rejects_garbage() {
  assert_eq!(stat_starttime_ticks(""), None);
  assert_eq!(stat_starttime_ticks("12345 (x) R"), None);
  assert_eq!(stat_starttime_ticks("12345 R 1 2 3"), None);
}

/// Tick math is exact and saturating.
#[cfg(target_os = "linux")]
#[test]
fn start_ms_math_is_exact() {
  assert_eq!(
    start_ms_from_ticks(1_700_000_000_000, 500, 100),
    1_700_000_005_000
  );
  assert_eq!(start_ms_from_ticks(u64::MAX, u64::MAX, 1), u64::MAX);
}
