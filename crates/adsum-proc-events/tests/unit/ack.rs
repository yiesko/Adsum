//! Wire-format tests for the subscription-refusal shapes (their offsets
//! are easy to drift; Linux only — the wire format only exists there).

use super::*;

/// `nlmsg_flags` bit marking a multipart reply.
const NLM_F_MULTI: u16 = 0x2;
/// `nlmsg_type` of the marker that wraps a kernel `cn_proc` reply.
const NLMSG_DONE: u16 = 0x3;

/// One netlink frame: `nlmsghdr` framing `body` under `msg_type`.
fn frame(msg_type: u16, body: &[u8]) -> Vec<u8> {
  let total = SIZE_NLMSGHDR + body.len();
  let mut buf = vec![0u8; total];
  buf[0..4].copy_from_slice(&(total as u32).to_le_bytes());
  buf[4..6].copy_from_slice(&msg_type.to_le_bytes());
  buf[6..8].copy_from_slice(&NLM_F_MULTI.to_le_bytes());
  buf[SIZE_NLMSGHDR..].copy_from_slice(body);
  buf
}

/// One kernel ack datagram in a given refusal shape; `NLMSG_DONE`
/// wraps the cn_proc data message, `proc_event` holds `what` with
/// `ack.err` at the pid offset (union base).
fn ack_frame(what: u32, ack_err: i32) -> Vec<u8> {
  let mut body = vec![0u8; SIZE_CN_MSG + 24];
  body[0..4].copy_from_slice(&CN_IDX_PROC.to_le_bytes());
  body[4..8].copy_from_slice(&CN_VAL_PROC.to_le_bytes());
  let event = SIZE_CN_MSG;
  body[event..event + 4].copy_from_slice(&what.to_le_bytes());
  body[event + 16..event + 20].copy_from_slice(&ack_err.to_le_bytes());
  frame(NLMSG_DONE, &body)
}

#[test]
fn nonzero_error_refuses() {
  let body = (-13i32).to_le_bytes();
  assert_eq!(subscribe_ack_error(&frame(NLMSG_ERROR, &body)), Some(-13));
}

#[test]
fn zero_error_ack_is_not_a_refusal() {
  let body = [0u8; 4];
  assert_eq!(subscribe_ack_error(&frame(NLMSG_ERROR, &body)), None);
}

#[test]
fn proc_event_none_ack_nonzero_err_refuses() {
  let datagram = ack_frame(PROC_EVENT_NONE, -111);
  assert_eq!(subscribe_ack_error(&datagram), Some(-111));
}

#[test]
fn proc_event_none_ack_zero_err_is_not_a_refusal() {
  let datagram = ack_frame(PROC_EVENT_NONE, 0);
  assert_eq!(subscribe_ack_error(&datagram), None);
}

#[test]
fn early_event_winning_the_ack_race_is_not_a_refusal() {
  // An EXEC (the usual winner) or any non-NONE `what` is traffic,
  // never an ack verdict.
  let datagram = ack_frame(0x0000_0002, -13);
  assert_eq!(subscribe_ack_error(&datagram), None);
}

#[test]
fn foreign_connector_traffic_is_not_a_refusal() {
  let mut body = vec![0u8; SIZE_CN_MSG + 24];
  body[0..4].copy_from_slice(&9u32.to_le_bytes());
  let datagram = frame(NLMSG_DONE, &body);
  assert_eq!(subscribe_ack_error(&datagram), None);
}

#[test]
fn short_and_corrupt_input_never_refuse() {
  // Below nlmsghdr, truncated framing, garbage lengths: unreadable,
  // not a refusal — the watcher skips it either way.
  assert_eq!(subscribe_ack_error(&[0, 1, 2]), None);
  let datagram = ack_frame(PROC_EVENT_NONE, -13);
  assert_eq!(subscribe_ack_error(&datagram[..SIZE_NLMSGHDR]), None);
  assert_eq!(subscribe_ack_error(&datagram[..8]), None);
}

#[test]
fn refusal_in_a_later_frame_still_wins() {
  // Zero-ack first (proceed), refusal second: the first bad message
  // in ANY frame of the datagram decides.
  let mut compound = frame(NLMSG_ERROR, &[0; 4]);
  let refusal = frame(NLMSG_ERROR, &(-1i32).to_le_bytes());
  compound.extend_from_slice(&refusal);
  assert_eq!(subscribe_ack_error(&compound), Some(-1));
}
