use super::*;

/// N3: the MessagePack form is derived once per frame, however many
/// consumers read it.
///
/// A broadcast used to parse the JSON and re-encode it for every
/// MessagePack consumer, so the cost scaled with the consumer count on
/// a path that sends the same bytes to all of them. The derivation
/// counter is what makes that observable — a cached-buffer pointer check
/// cannot fail, because returning `&Bytes` from `&self` already forces a
/// stable address.
#[test]
fn msgpack_is_derived_once_for_every_consumer() {
  let frame = Frame::new(r#"{"cmd":"CURRENT_USER_UPDATE","data":{"user":{"id":"7"}}}"#);
  for _ in 0..4 {
    assert!(frame.msgpack().is_some(), "encodes");
  }
  assert_eq!(
    frame.derivations(),
    1,
    "four consumers must cost one derive, not four"
  );
}

/// A frame nobody sends over MessagePack must not derive it at all:
/// encoding eagerly would charge every ACK — one JSON consumer — for
/// bytes nobody reads.
#[test]
fn an_unsent_msgpack_form_is_never_derived() {
  let frame = Frame::new(r#"{"cmd":"ACK","data":null}"#);
  assert_eq!(frame.derivations(), 0, "nothing derived yet");
  assert!(
    frame.msgpack_cell.get().is_none(),
    "the cell stays empty until a MessagePack consumer asks"
  );
}

/// The derived bytes are the same value the JSON carries: caching must
/// not change the wire form a consumer sees.
#[test]
fn derived_msgpack_round_trips_the_json_value() {
  let json = r#"{"cmd":"CURRENT_USER_UPDATE","data":{"user":{"id":"7"}}}"#;
  let frame = Frame::new(json);
  let bytes = frame.msgpack().expect("encodes");
  let back: serde_json::Value = rmp_serde::from_slice(bytes).expect("valid MessagePack");
  let want: serde_json::Value = serde_json::from_str(json).expect("valid JSON");
  assert_eq!(back, want);
}
