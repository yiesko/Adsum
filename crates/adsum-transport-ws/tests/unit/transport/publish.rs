use super::*;

///
/// The staged path used to stage first and flush immediately, which cost
/// two lock round-trips on the pending queue for every staged publish —
/// including the common case where the sink had room and the entry left
/// the queue in the very next statement.
#[tokio::test]
async fn a_publish_with_room_is_delivered_not_queued() {
  let (tx, mut rx) = mpsc::channel(4);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());

  assert!(sink.emit_or_stage_publish(&pending, big_publish(3)));

  assert!(
    pending.lock().unwrap().is_empty(),
    "a deliverable publish must not sit in the queue"
  );
  let got = rx.try_recv().expect("delivered to the sink");
  assert_eq!(got.args.as_ref().and_then(|a| a.pid), Some(3));
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
}

/// A full sink still stages, and the ordering invariant holds: the entry
/// joins the queue behind whatever the earlier flush could not drain.
#[tokio::test]
async fn a_full_sink_stages_the_publish_in_order() {
  // Cap-1 sink held full by a filler: nothing can be delivered.
  let (tx, mut rx) = mpsc::channel(1);
  tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
    .await
    .expect("filler");
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  // An older entry the earlier flush left behind (sink full).
  sink.stage_clear(&pending, &slot(7));

  assert!(sink.emit_or_stage_publish(&pending, big_publish(9)));

  assert_eq!(
    pending.lock().unwrap().len(),
    2,
    "both entries stay queued while the sink is full"
  );
  // Free the sink: the older entry must land first.
  rx.try_recv().expect("filler drains");
  sink.flush_pending(&pending);
  let first = rx.try_recv().expect("older entry first");
  assert_eq!(
    first.args.as_ref().and_then(|a| a.pid),
    Some(7),
    "a new publish must never overtake an older staged entry"
  );
}

/// A gone sink counts the shed and reports failure, like every other
/// emit path.
#[tokio::test]
async fn a_closed_sink_sheds_and_reports_failure() {
  let (tx, rx) = mpsc::channel::<ActivityCmd>(1);
  drop(rx);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());

  assert!(!sink.emit_or_stage_publish(&pending, big_publish(3)));
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
  assert!(pending.lock().unwrap().is_empty());
}
/// A deliverable publish must not touch the staged queue at all.
///
/// The outcome alone does not prove it — staging and immediately
/// draining leaves the queue empty either way. What differs is the
/// collateral: at the cap, staging sheds the oldest staged entry, which
/// was still owed delivery. Delivering directly leaves it queued.
#[tokio::test]
async fn a_deliverable_publish_leaves_the_queue_untouched() {
  // Sink starts full so the older entries can be staged, then frees up
  // without a flush: the queue sits at its cap, deliverable but queued.
  let (tx, mut rx) = mpsc::channel(1);
  tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
    .await
    .expect("filler");
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  for pid in 0..MAX_PENDING_COMMANDS as u64 {
    sink.stage_clear(&pending, &slot(pid));
  }
  assert_eq!(pending.lock().unwrap().len(), MAX_PENDING_COMMANDS);
  rx.try_recv().expect("free the sink");
  // No flush here, deliberately: the queue is full and the sink has room.

  assert!(sink.emit_or_stage_publish(&pending, big_publish(99)));

  let got = rx.try_recv().expect("the publish is delivered directly");
  assert_eq!(got.args.as_ref().and_then(|a| a.pid), Some(99));
  assert_eq!(
    sink.dropped.load(Ordering::Relaxed),
    0,
    "a deliverable publish must not shed an older staged entry"
  );
  assert_eq!(
    pending.lock().unwrap().len(),
    MAX_PENDING_COMMANDS,
    "the staged queue must be untouched"
  );
}
