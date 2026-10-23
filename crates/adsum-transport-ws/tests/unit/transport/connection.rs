use super::*;

/// Discord origins pass, missing origin passes, anything else is refused.
#[test]
fn origin_policy() {
  assert!(origin_allowed(None));
  assert!(origin_allowed(Some("https://discord.com")));
  assert!(origin_allowed(Some("https://canary.discord.com")));
  assert!(origin_allowed(Some("https://ptb.discord.com")));
  assert!(!origin_allowed(Some("https://evil.example")));
  assert!(!origin_allowed(Some("")));
}

/// The re-exported query parser keeps its legacy key/value behavior.
#[test]
fn query_params_match_legacy_semantics() {
  // Canonical parser lives in adsum-protocol (tested there); this pins
  // the import still resolves to the same behavior here.
  let uri = "/?v=1&client_id=abc";
  assert_eq!(query_param(uri, "v"), Some("1"));
  assert_eq!(query_param(uri, "client_id"), Some("abc"));
}

/// A4: the per-message snapshot shares the connection's client id
/// instead of copying it.
///
/// The snapshot is rebuilt for **every** message — `SUBSCRIBE` and
/// `GET_USER` included, which never use the id at all — so an owned
/// `String` here paid a deep copy on the whole message path. With the
/// id refcounted the clone is a counter bump and the buffer is shared.
/// Goes through [`SlotSnapshot::from_slot`] on purpose: building the
/// struct by hand here would leave the real clone site untested, and
/// a copy reintroduced there would pass.
#[test]
fn slot_snapshot_shares_the_connection_client_id() {
  let slot = ClientSlot {
    responder: Responder::for_test(),
    published: Vec::new(),
    query_client_id: Some(AppId::from("123456789012345678")),
  };
  let stored_ptr = slot
    .query_client_id
    .as_ref()
    .expect("client id")
    .as_ref()
    .as_ptr();

  let snapshot = SlotSnapshot::from_slot(&slot);

  let shared = snapshot.query_client_id.expect("client id");
  assert!(
    std::ptr::eq(shared.as_ref().as_ptr(), stored_ptr),
    "the per-message snapshot must share the connection id, never copy it"
  );
}

/// N12: a connection that forgets its last published pid releases the
/// `published` buffer instead of holding its high-water mark.
///
/// The list grows with what a multiplexing client publishes, so its
/// capacity is that client's peak — not the steady state. Replacing it
/// with a fresh `Vec` when it drains returns that buffer while the
/// connection is still open, which is where the per-connection
/// footprint would otherwise ratchet.
#[test]
fn forgetting_the_last_pid_releases_the_published_buffer() {
  let mut slot = ClientSlot {
    responder: adsum_ws::Responder::for_test(),
    published: Vec::new(),
    query_client_id: None,
  };
  for pid in 1..=handlers::MAX_TRACKED_PIDS as u64 {
    let admission = slot.note_published(
      Some(adsum_types::AppId::from("app-1")),
      pid,
      Value::String(format!("n{pid}")),
    );
    assert!(
      matches!(admission, Admission::Admitted { .. }),
      "each pid fits"
    );
  }
  let peak = slot.published.capacity();
  assert!(
    peak >= handlers::MAX_TRACKED_PIDS,
    "buffer grew to the peak"
  );

  for pid in 1..=handlers::MAX_TRACKED_PIDS as u64 {
    slot.forget_published(pid);
  }
  assert!(slot.published.is_empty());
  assert_eq!(
    slot.published.capacity(),
    0,
    "an emptied publication list must hand its buffer back"
  );
}

/// A list that still holds entries keeps its buffer: releasing per
/// removal would trade a one-off retention for per-publish reallocation.
#[test]
fn a_partially_forgotten_list_keeps_its_buffer() {
  let mut slot = ClientSlot {
    responder: adsum_ws::Responder::for_test(),
    published: Vec::new(),
    query_client_id: None,
  };
  slot.note_published(
    Some(adsum_types::AppId::from("app-1")),
    1,
    Value::String("n1".into()),
  );
  slot.note_published(
    Some(adsum_types::AppId::from("app-1")),
    2,
    Value::String("n2".into()),
  );
  let before = slot.published.capacity();

  slot.forget_published(1);

  assert_eq!(slot.published.len(), 1);
  assert!(
    slot.published.capacity() >= before,
    "a still-populated list must not be reallocated"
  );
}
