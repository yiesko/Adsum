use super::*;

/// N29: one oversized frame must not pin its buffer for the rest of the
/// connection's life.
///
/// The read buffer is reused across frames (`clear()`, not a fresh
/// `String`), so it keeps whatever the largest frame needed. The payload
/// cap is 1 MiB, and a pump serves one connection each: a single client
/// that ever sent a big frame would hold that much for its whole
/// lifetime, and the high-water mark would never come back down.
#[test]
fn an_oversized_frame_releases_the_buffer() {
  let mut message = String::with_capacity(MAX_IPC_PAYLOAD as usize);
  message.push_str(&"x".repeat(MAX_IPC_PAYLOAD as usize));
  release_message_capacity(&mut message);
  assert!(
    message.capacity() <= MESSAGE_BUFFER_FLOOR,
    "an oversized frame must release its buffer, kept {} bytes",
    message.capacity()
  );
}

/// The release must be conditional: steady-state frames are a few
/// hundred bytes, and reallocating on every one of those would trade a
/// one-off retention for per-frame churn.
#[test]
fn a_steady_frame_keeps_its_buffer() {
  let mut message = String::with_capacity(256);
  message.push_str(&"x".repeat(256));
  let before = message.capacity();
  release_message_capacity(&mut message);
  assert!(
    message.capacity() >= before,
    "a small buffer must not be shrunk on every frame"
  );
}

/// The buffer must stay usable after a release: the next frame refills
/// the same `String`, and the capacity rule is re-evaluated per frame.
#[test]
fn the_buffer_is_reusable_after_a_release() {
  let mut message = String::with_capacity(MAX_IPC_PAYLOAD as usize);
  message.push_str(&"x".repeat(MAX_IPC_PAYLOAD as usize));
  release_message_capacity(&mut message);
  message.clear();
  message.push_str("next-frame");
  assert_eq!(message, "next-frame");
}
