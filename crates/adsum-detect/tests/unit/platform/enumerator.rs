use super::*;

/// Digits parse, including a long pid, without allocating a String or
/// a PathBuf per entry.
#[test]
fn proc_pid_parses_digits() {
  assert_eq!(parse_proc_pid(b"1"), Some(1));
  assert_eq!(parse_proc_pid(b"12345"), Some(12_345));
  assert_eq!(parse_proc_pid(b"4294967294"), Some(u32::MAX as u64 - 1));
  assert_eq!(parse_proc_pid(b""), None);
}

/// Non-numeric names are skipped, not fatal: `/proc` also holds
/// non-pid entries and this must never panic.
#[test]
fn proc_pid_rejects_non_numeric() {
  assert_eq!(parse_proc_pid(b"self"), None);
  assert_eq!(parse_proc_pid(b"cpuinfo"), None);
  assert_eq!(parse_proc_pid(b"12a"), None);
  assert_eq!(parse_proc_pid(b"a12"), None);
  assert_eq!(parse_proc_pid(b"-1"), None);
  assert_eq!(parse_proc_pid(b" 12"), None);
  // Non-UTF-8 bytes are just non-digits.
  assert_eq!(parse_proc_pid(&[0xff, 0xfe]), None);
}

/// An overflowing pid is rejected rather than wrapped: a wrapped value
/// would silently point at another process.
#[test]
fn proc_pid_rejects_overflow() {
  assert_eq!(parse_proc_pid(b"18446744073709551616"), None);
  assert_eq!(parse_proc_pid(b"99999999999999999999999"), None);
}
