//! `uid_map` and `NSpid` identity tests (Linux only: procfs only exists
//! there).

use super::*;

#[test]
fn full_identity_map_is_initial() {
  // Host shell (`/proc/self/uid_map`): the full identity range.
  assert!(is_initial_uid_map("0            0   4294967295\n"));
}

#[test]
fn varied_spacing_still_identity() {
  // All whitespace splits equally: tabs, runs of spaces, blank padding.
  assert!(is_initial_uid_map("\t0\t\t 0\t4294967295  "));
  assert!(is_initial_uid_map("0\n0\n4294967295"));
}

#[test]
fn unshare_ranged_map_is_not_initial() {
  // `unshare -U -r`: 0→1000 sized 1 — a single-uid mapping, not identity.
  assert!(!is_initial_uid_map("0 1000 1"));
}

#[test]
fn re_mapped_range_is_not_initial() {
  assert!(!is_initial_uid_map("1000 1000 1"));
}

#[test]
fn garbage_tokens_are_not_initial() {
  assert!(!is_initial_uid_map("x y z"));
}

#[test]
fn missing_tokens_are_not_initial() {
  assert!(!is_initial_uid_map("0 0"));
  assert!(!is_initial_uid_map(""));
}

#[test]
fn mapping_beyond_identity_is_not_initial() {
  // A second line maps another range: identity head, non-initial whole.
  assert!(!is_initial_uid_map("0 0 4294967295\n1000 1000 1\n"));
}

#[test]
fn single_nspid_value_is_initial() {
  // The initial PID namespace has one pid value; older procfs omits the
  // line entirely, which is also the initial namespace.
  assert!(is_initial_nspid("Name:\tadsum\nNSpid:\t1234\n"));
  assert!(is_initial_nspid("Name:\tadsum\n"));
}

#[test]
fn nested_pid_namespace_is_not_initial() {
  // A process in a nested PID namespace has one pid per visible level.
  assert!(!is_initial_nspid("NSpid:\t1234\t42\n"));
  assert!(!is_initial_nspid("NSpid: 1234 42 7"));
}

#[test]
fn empty_or_malformed_nspid_is_not_initial() {
  assert!(!is_initial_nspid("NSpid:\n"));
  assert!(!is_initial_nspid("NSpid:   \n"));
}
