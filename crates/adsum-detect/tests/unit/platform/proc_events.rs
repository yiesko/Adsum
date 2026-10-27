//! Retry policy: the delay doubles per consecutive failure, caps at five
//! minutes, and a watch that proved delivery starts a fresh streak.

use super::*;

#[test]
fn first_failure_retries_after_the_initial_delay() {
  assert_eq!(retry_delay(1), Duration::from_secs(5));
}

#[test]
fn delay_doubles_per_consecutive_failure() {
  assert_eq!(retry_delay(2), Duration::from_secs(10));
  assert_eq!(retry_delay(3), Duration::from_secs(20));
  assert_eq!(retry_delay(4), Duration::from_secs(40));
}

#[test]
fn delay_caps_at_the_ceiling() {
  assert_eq!(retry_delay(7), RETRY_CEILING);
  assert_eq!(retry_delay(u32::MAX), RETRY_CEILING);
}

#[test]
fn live_watch_resets_the_streak() {
  // After a healthy watch, the next failure must retry like a new one.
  assert_eq!(next_failures(9, true), 1);
  assert_eq!(retry_delay(next_failures(9, true)), RETRY_INITIAL);
}

#[test]
fn failed_watch_advances_the_streak() {
  assert_eq!(next_failures(0, false), 1);
  assert_eq!(next_failures(4, false), 5);
  assert_eq!(next_failures(u32::MAX, false), u32::MAX);
}

#[test]
fn delay_is_monotonic_through_the_cap() {
  let mut previous = Duration::ZERO;
  for failures in 1..=20 {
    let delay = retry_delay(failures);
    assert!(delay >= previous, "delay must not shrink at {failures}");
    previous = delay;
  }
  assert_eq!(previous, RETRY_CEILING);
}
