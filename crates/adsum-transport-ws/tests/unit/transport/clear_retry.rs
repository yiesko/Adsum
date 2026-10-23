use super::*;

/// A briefly-full sink still delivers via retry (no shed on transients).
#[tokio::test]
async fn clear_retry_survives_a_briefly_full_sink() {
  // Cap-1 sink held full: the retry lands once space frees inside the
  // budget (a single 250ms attempt would shed it).
  let (tx, mut rx) = mpsc::channel(1);
  tx.send_timeout(
    handlers::clear_for_slot(&handlers::PublishedSlot {
      app_id: None,
      pid: 1,
      nonce: Value::Null,
    }),
    SINK_SEND_TIMEOUT,
  )
  .await
  .expect("filler");
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let cmd = handlers::clear_for_slot(&handlers::PublishedSlot {
    app_id: None,
    pid: 7,
    nonce: Value::Null,
  });
  // Drain once after 400ms, then hold the receiver open (dropping it
  // would close the channel and turn the retry into a `Closed` shed).
  let drain = tokio::spawn(async move {
    tokio::time::sleep(Duration::from_millis(400)).await;
    rx.recv().await.expect("filler");
    std::future::pending::<()>().await;
  });
  let deadline = std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET;
  assert!(sink.emit_clear_retry(cmd, deadline).await);
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
  drain.abort();
}

/// A closed sink fails fast-ish and counts the shed clear.
#[tokio::test]
async fn clear_retry_gives_up_past_the_budget() {
  // Closed sink: no capacity will ever return — fail fast-ish, counted.
  let (tx, rx) = mpsc::channel::<ActivityCmd>(1);
  drop(rx);
  let sink = Sink {
    tx,
    dropped: Arc::new(AtomicU64::new(0)),
  };
  let cmd = handlers::clear_for_slot(&handlers::PublishedSlot {
    app_id: None,
    pid: 7,
    nonce: Value::Null,
  });
  let deadline = std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET;
  assert!(!sink.emit_clear_retry(cmd, deadline).await);
  assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
}
