use super::*;

/// Staged entries never exceed the bound; every shed is counted.
#[test]
fn staged_entries_stay_bounded_and_counted() {
  let (tx, _rx) = mpsc::channel::<ActivityCmd>(1);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  for pid in 0..(MAX_PENDING_COMMANDS + 5) as u64 {
    sink.stage_clear(&pending, &slot(pid));
  }
  // Bounded history, and every shed entry counted (never silent).
  assert!(pending.lock().unwrap().len() <= MAX_PENDING_COMMANDS);
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 5);
}

/// Publishes stage too, so a wedged sink parks them instead of dropping.
#[test]
fn staged_publish_keeps_its_command() {
  let (tx, _rx) = mpsc::channel::<ActivityCmd>(1);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  sink.stage_publish(&pending, big_publish(5));
  pending
    .lock()
    .unwrap()
    .retain(|entry| matches!(entry.payload, PendingPayload::Publish(_)));
  assert_eq!(pending.lock().unwrap().len(), 1);
}

/// Flush delivers oldest-first and stops at the first full queue.
#[tokio::test]
async fn flush_preserves_order_and_stops_when_full() {
  // Cap-1 sink held full by a filler: flush delivers nothing, staged intact.
  let (tx, mut rx) = mpsc::channel(1);
  tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
    .await
    .expect("filler");
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  for pid in [7u64, 8, 9] {
    sink.stage_clear(&pending, &slot(pid));
  }
  sink.flush_pending(&pending);
  assert_eq!(pending.lock().unwrap().len(), 3);
  // Free slots one by one: staged entries land oldest-first.
  for pid in [1u64, 7, 8] {
    let cmd = rx.try_recv().expect("slot frees in order");
    assert_eq!(cmd.args.as_ref().and_then(|a| a.pid), Some(pid));
    sink.flush_pending(&pending);
  }
  assert!(pending.lock().unwrap().is_empty());
  let last = rx.try_recv().expect("final staged entry");
  assert_eq!(last.args.as_ref().and_then(|a| a.pid), Some(9));
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
}

/// A clear a full sink refuses keeps its slim form when it goes back.
///
/// The flush offers each entry as a full command, so a refused clear used
/// to come back as a boxed publish: same bytes, same place in the queue,
/// but the ~1 KB command retained per entry for as long as the sink stays
/// full. A wedged sink is exactly the case the slim form exists for, and
/// it runs on every pump event while the queue is non-empty.
#[tokio::test]
async fn a_refused_clear_is_requeued_slim() {
  // Cap-1 sink held full by a filler: the flush delivers nothing, so
  // every staged entry takes the refused path.
  let (tx, _rx) = mpsc::channel(1);
  tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
    .await
    .expect("filler");
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  for pid in [7u64, 8] {
    sink.stage_clear(&pending, &slot(pid));
  }
  // Each entry carries its own retry window; the requeue must not stamp
  // a new one, or the entry the oldest retry task is waiting on silently
  // keeps a stranger's window.
  let deadlines: Vec<_> = pending.lock().unwrap().iter().map(|e| e.deadline).collect();

  sink.flush_pending(&pending);

  let pending = pending.lock().unwrap();
  assert_eq!(pending.len(), 2, "a full queue keeps every entry");
  let pids: Vec<u64> = pending
    .iter()
    .map(|entry| match &entry.payload {
      PendingPayload::Clear(clear) => clear.pid,
      PendingPayload::Publish(_) => {
        panic!("a refused clear came back as a publish, retaining the full command")
      }
    })
    .collect();
  assert_eq!(
    pids,
    vec![7, 8],
    "both clears stay slim and keep their order"
  );
  assert_eq!(
    deadlines,
    pending.iter().map(|e| e.deadline).collect::<Vec<_>>(),
    "the requeue keeps each entry's own deadline"
  );
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
}

/// Expiry removes only lapsed entries, keeping fresh retry windows.
#[test]
fn expiry_sheds_only_lapsed_entries() {
  let (tx, _rx) = mpsc::channel::<ActivityCmd>(1);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  // One long-lapsed entry and one fresh entry, built directly (staging
  // always stamps `now + budget`, which no deterministic test can outwait).
  let past = std::time::Instant::now() - Duration::from_secs(1);
  let future = std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET;
  for (pid, deadline) in [(1u64, past), (2u64, future)] {
    pending.lock().unwrap().push_back(PendingEntry {
      payload: PendingPayload::Clear(PendingClear::from_slot(&slot(pid))),
      deadline,
    });
  }
  sink.expire_pending(&pending);
  // Only the lapsed entry shed (counted); the fresh one survives with
  // its own retry window intact.
  let pending = pending.lock().unwrap();
  assert_eq!(pending.len(), 1);
  let PendingPayload::Clear(clear) = &pending[0].payload else {
    panic!("expected a clear");
  };
  assert_eq!(clear.pid, 2);
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
}

/// N13: the shutdown drain takes the queue in batches, not one entry per
/// lock round-trip.
///
/// It used to lock, pop one entry, unlock, await its bounded retry, and
/// repeat — so a wedge at shutdown paid one lock acquisition per entry
/// and the queue kept shifting under each pop. Taking the whole queue
/// leaves one lock and an empty queue behind, and the per-entry await
/// (each clear keeps its own deadline) is unchanged.
#[tokio::test]
async fn a_drain_take_empties_the_queue_in_one_lock() {
  let (tx, mut rx) = mpsc::channel(8);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  for pid in [1u64, 2, 3] {
    sink.stage_clear(&pending, &slot(pid));
  }

  // One take replaces the whole queue; nothing is lost and the shared
  // queue is empty, so a concurrent stager cannot be stranded.
  let batch = take_pending(&pending);
  assert_eq!(batch.len(), 3, "the whole queue leaves in one take");
  assert!(
    pending.lock().unwrap().is_empty(),
    "the shared queue must be empty after the take"
  );
  // Deliver the batch: order is the queue's, unchanged by taking it.
  for entry in batch {
    assert!(
      sink
        .emit_clear_retry(entry.payload.into_cmd(), entry.deadline)
        .await
    );
  }
  for pid in [1u64, 2, 3] {
    let cmd = rx.try_recv().expect("drained in order");
    assert_eq!(cmd.args.as_ref().and_then(|a| a.pid), Some(pid));
  }
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
}

/// A second take after a late staging still drains it: the drain loops on
/// batches rather than assuming the first take finished the job.
#[tokio::test]
async fn a_second_take_still_drains_late_entries() {
  let (tx, mut rx) = mpsc::channel(8);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let pending = Mutex::new(PendingQueue::new());
  sink.stage_clear(&pending, &slot(1));
  assert_eq!(take_pending(&pending).len(), 1);
  assert!(pending.lock().unwrap().is_empty());

  // A straggler staged after the first batch (nothing does this today,
  // but the drain must not silently drop it).
  sink.stage_clear(&pending, &slot(9));
  let second = take_pending(&pending);
  assert_eq!(second.len(), 1, "a late entry is still drained");
  assert_eq!(
    second.front().map(|entry| match &entry.payload {
      PendingPayload::Clear(clear) => clear.pid,
      PendingPayload::Publish(cmd) => cmd.args.as_ref().and_then(|a| a.pid).unwrap_or_default(),
    }),
    Some(9)
  );
  let _ = rx.try_recv();
}
