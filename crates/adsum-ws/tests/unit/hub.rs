use super::*;

/// Fixture responder with a controllable outbox of `channel` slots.
fn responder_with(channel: usize) -> (Responder, mpsc::Receiver<Cmd>) {
  let (tx, rx) = mpsc::channel(channel);
  let responder = Responder::new(
    tx,
    7,
    Arc::new(ConnectionDetails {
      peer: "127.0.0.1:1".parse().unwrap(),
      headers: Arc::new(HeaderMap::new()),
      uri: Arc::from("/"),
    }),
  );
  (responder, rx)
}

/// A full outbox reports `Full` instead of blocking or growing memory.
#[test]
fn try_send_full_does_not_block() {
  let (r, _rx) = responder_with(1);
  r.try_send(Message::from("one")).unwrap();
  assert_eq!(r.try_send(Message::from("two")), Err(TrySendError::Full));
}

/// Sends after the connection task exits report `Closed`.
#[test]
fn try_send_closed_after_task_exit() {
  let (r, rx) = responder_with(1);
  drop(rx);
  assert_eq!(r.try_send(Message::from("x")), Err(TrySendError::Closed));
}

/// `is_closed` flips exactly when the connection task exits.
#[test]
fn closed_probe_tracks_task_exit() {
  let (r, rx) = responder_with(1);
  assert!(!r.is_closed());
  drop(rx);
  assert!(r.is_closed());
}

/// Async send and close succeed while the task drains (best-effort).
#[tokio::test]
async fn send_async_and_close_are_best_effort() {
  let (r, _rx) = responder_with(8);
  r.send_async(Message::from("hi")).await.unwrap();
  r.close(CloseCode::Normal).await;
}

/// Bounded sends fail fast on a full outbox and recover after drain.
#[tokio::test]
async fn send_timeout_gives_up_on_a_full_outbox() {
  let (r, mut rx) = responder_with(1);
  r.try_send(Message::from("filler")).unwrap();
  let started = std::time::Instant::now();
  let result = r
    .send_timeout(Message::from("reply"), Duration::from_millis(50))
    .await;
  assert!(result.is_err(), "full outbox must fail the bounded wait");
  assert!(
    started.elapsed() < Duration::from_secs(2),
    "must not wait for capacity"
  );
  // Draining frees the slot: the next bounded send succeeds.
  assert!(rx.try_recv().is_ok());
  r.send_timeout(Message::from("reply"), Duration::from_millis(50))
    .await
    .unwrap();
}

/// The hub yields queued events in order, then `None` after drain.
#[tokio::test]
async fn hub_streams_events_in_order() {
  use futures_util::StreamExt;
  let (tx, rx) = mpsc::channel(8);
  let mut hub = EventHub::new(rx);
  tx.send(Event::Disconnect(3, DisconnectReason::Clean))
    .await
    .unwrap();
  drop(tx);
  let first = hub.next().await.unwrap();
  assert!(matches!(first, Event::Disconnect(3, _)));
  assert!(hub.next_event().await.is_none());
}
