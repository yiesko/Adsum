use super::*;

/// Full sinks shed counted instead of blocking the caller.
#[test]
fn emit_never_blocks_and_counts() {
  let (sink, mut rx) = EventSink::bounded(1);
  sink.emit(ActivityCmd::empty());
  sink.emit(ActivityCmd::empty());
  assert_eq!(sink.dropped_total(), 1);
  assert!(rx.try_recv().is_ok());
}

/// Clears wait briefly for space instead of shedding like commands.
#[test]
fn emit_clear_survives_a_full_queue() {
  // Clears are final (no later event repairs a dropped one): a full
  // queue must delay them briefly, never shed them like ordinary
  // commands.
  let (sink, mut rx) = EventSink::bounded(1);
  sink.emit(ActivityCmd::empty()); // queue full
  let clearer = sink.clone();
  let handle = std::thread::spawn(move || clearer.emit_clear(ActivityCmd::empty()));
  std::thread::sleep(std::time::Duration::from_millis(50));
  assert!(rx.try_recv().is_ok(), "filler arrives");
  // The retry loop lands within milliseconds of the freed slot: poll
  // with a generous deadline instead of asserting instantly.
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
  loop {
    if rx.try_recv().is_ok() {
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "clear follows once space frees"
    );
    std::thread::sleep(std::time::Duration::from_millis(1));
  }
  handle.join().expect("clear delivered");
  assert_eq!(sink.dropped_total(), 0);
}
