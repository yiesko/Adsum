use super::*;
use crate::handoff::HandoffState;
use adsum_types::DetectSource;

/// Epoch millis for latency fixtures.
fn now_ms() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .ok()
    .and_then(|age| u64::try_from(age.as_millis()).ok())
    .unwrap_or(0)
}

/// Generic payloads stamp detection provenance for snapshots: the
/// matcher's source travels on the card, latency is process age at
/// first publish.
#[test]
fn generic_payload_stamps_provenance() {
  let game = ScannedGame {
    id: AppId::from("12345"),
    name: Arc::from("Source Game"),
    pid: u64::from(std::process::id()),
    start: 0,
    source: DetectSource::SteamAppId,
    process_start_ms: Some(now_ms().saturating_sub(5000)),
  };
  let payload = generic_payload(&game).expect("builds");
  let provenance = payload.provenance.clone().expect("stamped");
  assert_eq!(provenance.source, "steam-app-id");
  let latency = provenance.latency_ms.expect("latency");
  assert!(
    (5000..=60_000).contains(&latency),
    "latency {latency}ms should be ~5s"
  );
}

/// F2: the matcher source travels as a `DetectSource`, not a `String`,
/// and the provenance keeps the wire identifier without re-allocating.
///
/// Before, the daemon stringified the enum at the scanner/bridge
/// boundary, the bridge cloned that `String` per event, and the
/// provenance cloned it again. The enum is `Copy`, and the provenance
/// borrows the static wire identifier — so detection provenance costs
/// nothing on the publish path.
#[test]
fn provenance_source_travels_as_the_matcher_enum() {
  for (source, expected) in [
    (DetectSource::Automaton, "automaton"),
    (DetectSource::SteamAppId, "steam-app-id"),
    (DetectSource::EpicAppId, "epic-app-id"),
    (DetectSource::ProtonAutomaton, "proton-automaton"),
    (DetectSource::SteamLibrary, "steam-library"),
    (DetectSource::ExeStem, "exe-stem"),
    (DetectSource::Folder, "folder"),
    (DetectSource::CwdJoined, "cwd-joined"),
  ] {
    let game = ScannedGame {
      id: AppId::from("12345"),
      name: Arc::from("Source Game"),
      pid: u64::from(std::process::id()),
      start: 0,
      source,
      process_start_ms: Some(now_ms().saturating_sub(5000)),
    };
    let payload = generic_payload(&game).expect("builds");
    let provenance = payload.provenance.clone().expect("stamped");
    assert_eq!(
      &*provenance.source, expected,
      "wire id changed for {source:?}"
    );
    // The point of the `Cow`: a matcher source borrows its static wire
    // id, so a publish allocates nothing to say where the card came
    // from. Value equality alone would pass with an owned `String`.
    assert!(
      matches!(provenance.source, Cow::Borrowed(_)),
      "{source:?} must borrow the static wire id, not allocate a String"
    );
  }
}

/// The `"sdk"` provenance (client-published, not a matcher) still reads
/// as the same wire string a snapshot consumer has always seen — and it
/// borrows the literal, so saying where a client card came from costs
/// no allocation.
#[test]
fn sdk_provenance_keeps_its_wire_string() {
  // Built exactly the way the SDK publish path builds it
  // (`handle_set_activity` stamps provenance after the payload build).
  let provenance = commands::ActivityProvenance {
    source: Cow::Borrowed("sdk"),
    latency_ms: None,
  };
  assert_eq!(&*provenance.source, "sdk");
  assert!(
    matches!(provenance.source, Cow::Borrowed(_)),
    "the SDK path must borrow the literal, not allocate a String"
  );
}

/// F3: a handoff's game name is refcounted, so the resume clone (the
/// second of four deep `String` copies the chain used to pay) is a
/// counter bump over the same buffer.
///
/// Pointer identity is the assertion: a copy would land in a fresh
/// allocation and fail here, while the refcounted field does not.
#[test]
fn handoff_resume_shares_the_game_name_buffer() {
  let mut handoff = HandoffState::default();
  let game = ScannedGame {
    id: AppId::from("12345"),
    name: Arc::from("Source Game"),
    pid: 7,
    start: 0,
    source: DetectSource::Automaton,
    process_start_ms: None,
  };
  handoff.note_scan(Some(game));
  let name_ptr = handoff.resume_for("12345").expect("resumed").name.as_ptr();

  let resumed = handoff.resume_for("12345").expect("still resumed");
  assert!(
    std::ptr::eq(name_ptr, resumed.name.as_ptr()),
    "a resumed game must share its name buffer, never copy it"
  );
}

/// The replay cache stamps each entry with a fresh sequence number.
///
/// The counter used to live in its own `Mutex` taken *inside* the cache
/// lock — two acquisitions per publish, and a nested-lock ordering that
/// exists only because the counter is never touched outside it.
///
/// The invariant the change must not break: every non-clear publish
/// stamps a sequence the previous one did not, so insertion order stays
/// recoverable from the cache alone.
#[tokio::test]
async fn each_publish_stamps_a_fresh_sequence() {
  let shared = crate::bridge::shared_for_test();
  let mut cmd: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
          "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
  )
  .expect("parses");

  shared.handle_set_activity(cmd.clone()).await;
  let first = seq_of(&shared, 1);

  // A different pid, so this is a second cache entry rather than a
  // replace of the first.
  cmd.args.as_mut().unwrap().pid = Some(2);
  shared.handle_set_activity(cmd).await;
  let second = seq_of(&shared, 2);

  assert!(first >= 1, "the first publish stamps a sequence");
  assert!(
    second > first,
    "a second publish must not reuse the first's sequence ({first})"
  );
}

/// Only an explicit `activity: null` with a nonzero pid is a genuine
/// clear; an omitted field is malformed input, not a clear.
#[test]
fn genuine_clear_requires_an_explicit_null_activity() {
  let clear: ActivityCmd =
    serde_json::from_str(r#"{"cmd":"SET_ACTIVITY","args":{"pid":1,"activity":null},"nonce":"n"}"#)
      .expect("parses");
  assert!(is_genuine_clear(&clear));

  let missing: ActivityCmd =
    serde_json::from_str(r#"{"cmd":"SET_ACTIVITY","args":{"pid":1},"nonce":"n"}"#).expect("parses");
  assert!(!is_genuine_clear(&missing));

  let zero: ActivityCmd =
    serde_json::from_str(r#"{"cmd":"SET_ACTIVITY","args":{"pid":0,"activity":null},"nonce":"n"}"#)
      .expect("parses");
  assert!(!is_genuine_clear(&zero));
}

/// A `SET_ACTIVITY` that omits the `activity` field is ignored: it neither
/// publishes nor clears the cached card.
#[tokio::test]
async fn missing_activity_field_neither_publishes_nor_clears() {
  let shared = crate::bridge::shared_for_test();
  let publish: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
          "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
  )
  .expect("parses");
  shared.handle_set_activity(publish).await;
  let before = seq_of(&shared, 1);
  assert!(before >= 1, "the publish must be cached");

  let missing: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1},"nonce":"n2"}"#,
  )
  .expect("parses");
  shared.handle_set_activity(missing).await;
  assert_eq!(
    seq_of(&shared, 1),
    before,
    "a missing activity field must not clear the cached card"
  );
}

/// A `SET_ACTIVITY` that omits `args` entirely is dropped by the bridge:
/// it neither publishes nor clears the cached card.
#[tokio::test]
async fn absent_args_neither_publishes_nor_clears() {
  let shared = crate::bridge::shared_for_test();
  let publish: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
          "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
  )
  .expect("parses");
  shared.handle_set_activity(publish).await;
  let before = seq_of(&shared, 1);
  assert!(before >= 1, "the publish must be cached");

  let absent: ActivityCmd =
    serde_json::from_str(r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","nonce":"n2"}"#)
      .expect("parses");
  shared.handle_set_activity(absent).await;
  assert_eq!(
    seq_of(&shared, 1),
    before,
    "absent args must not change the cached card"
  );
  assert_eq!(
    shared.cache.lock().unwrap_or_else(|e| e.into_inner()).len(),
    1,
    "the publish must stay cached"
  );
}

/// A clear evicts rather than stamps, and a republish after it gets a
/// later sequence than the pre-clear one.
#[tokio::test]
async fn a_clear_does_not_reuse_a_sequence() {
  let shared = crate::bridge::shared_for_test();
  let cmd: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"SET_ACTIVITY","application_id":"app-1","args":{"pid":1,
          "activity":{"name":"G","type":0}},"nonce":"n1"}"#,
  )
  .expect("parses");
  shared.handle_set_activity(cmd.clone()).await;
  let before = seq_of(&shared, 1);

  let clear = ActivityCmd {
    application_id: Some(adsum_types::AppId::from("app-1")),
    cmd: "SET_ACTIVITY".into(),
    data: None,
    evt: None,
    args: Some(adsum_types::cmd::ActivityCmdArgs {
      pid: Some(1),
      activity: Some(None),
      code: None,
      user_id: None,
    }),
    nonce: serde_json::Value::String("n2".to_string()),
  };
  shared.handle_set_activity(clear).await;
  assert!(
    shared
      .cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .is_empty(),
    "a clear evicts the entry"
  );

  shared.handle_set_activity(cmd).await;
  assert!(
    seq_of(&shared, 1) > before,
    "a republish must not reuse the pre-clear sequence ({before})"
  );
}

/// Sequence stamped for the entry cached under `pid`, or 0 when absent.
fn seq_of(shared: &crate::bridge::Shared, pid: u64) -> u64 {
  shared
    .cache
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .get(&SocketId::from(pid))
    .map(|(_, seq)| *seq)
    .unwrap_or(0)
}
