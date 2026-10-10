use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::Value;

use rsrpc_protocol::commands::{
  RecentActivities, activity_fingerprint, cached_activity, current_user_update, generic_ack,
  rpc_error, set_activity_response, subscribe_ack, unsupported_command, user_response,
};
use rsrpc_types::AppId;
use rsrpc_types::cmd::ActivityCmd;
use rsrpc_types::user::RpcUser;

fn parse_cmd(json: &str) -> ActivityCmd {
  serde_json::from_str(json).expect("failed to parse test command")
}

#[test]
fn subscribe_ack_echoes_cmd_evt_and_nonce() {
  let cmd =
    parse_cmd(r#"{"cmd":"SUBSCRIBE","evt":"VOICE_STATE_UPDATE","nonce":"abc","args":null}"#);

  let reply: Value = serde_json::from_str(&subscribe_ack(&cmd)).expect("valid json");

  assert_eq!(reply["cmd"], "SUBSCRIBE");
  assert_eq!(reply["data"]["evt"], "VOICE_STATE_UPDATE");
  assert!(reply["evt"].is_null());
  assert_eq!(reply["nonce"], "abc");
}

#[test]
fn rpc_error_carries_code_and_message() {
  let reply: Value = serde_json::from_str(&rpc_error(
    "INVITE_BROWSER",
    &Value::String("n".to_string()),
    4011,
    "Invalid invite id",
  ))
  .expect("valid json");

  assert_eq!(reply["cmd"], "INVITE_BROWSER");
  assert_eq!(reply["data"]["code"], 4011);
  assert_eq!(reply["data"]["message"], "Invalid invite id");
  assert_eq!(reply["evt"], "ERROR");
  assert_eq!(reply["nonce"], "n");
}

#[test]
fn generic_ack_confirms_receipt_without_claiming_success() {
  let cmd = parse_cmd(r#"{"cmd":"DEEP_LINK","nonce":"7","args":null}"#);

  let reply: Value = serde_json::from_str(&generic_ack(&cmd)).expect("valid json");

  assert_eq!(reply["cmd"], "DEEP_LINK");
  assert!(reply["data"].is_null());
  assert!(reply["evt"].is_null());
  assert_eq!(reply["nonce"], "7");
}

#[test]
fn user_response_returns_identity_or_null() {
  let cmd = parse_cmd(r#"{"cmd":"GET_USER","nonce":"g1","args":null}"#);
  let user = RpcUser::default();

  let hit: Value = serde_json::from_str(&user_response(&cmd, Some(&user))).expect("valid json");
  assert_eq!(hit["cmd"], "GET_USER");
  assert_eq!(hit["data"]["id"], "1045800378228281345");
  assert!(hit["evt"].is_null());
  assert_eq!(hit["nonce"], "g1");

  // Unknown id: official "user object or null".
  let miss: Value = serde_json::from_str(&user_response(&cmd, None)).expect("valid json");
  assert!(miss["data"].is_null());
}

#[test]
fn current_user_update_is_a_dispatch_with_the_user_inside() {
  let frame: Value =
    serde_json::from_str(&current_user_update(&RpcUser::default())).expect("valid json");

  assert_eq!(frame["cmd"], "DISPATCH");
  assert_eq!(frame["evt"], "CURRENT_USER_UPDATE");
  assert_eq!(frame["data"]["username"], "arRPC");
  assert!(frame["nonce"].is_null());
}

#[test]
fn unsupported_commands_map_to_official_codes() {
  // OAuth needs the in-client flow + app secret.
  assert_eq!(
    unsupported_command("AUTHORIZE"),
    Some((5000, "Authorization requires the real Discord client"))
  );
  assert_eq!(
    unsupported_command("AUTHENTICATE"),
    Some((5000, "Authorization requires the real Discord client"))
  );
  // Invites target live sessions on the real client.
  assert_eq!(
    unsupported_command("SEND_ACTIVITY_JOIN_INVITE"),
    Some((
      5006,
      "No eligible activity: invites require the real Discord client"
    ))
  );
  // Voice/guilds/overlay/store: real-client state only.
  let (code, _) = unsupported_command("SELECT_VOICE_CHANNEL").expect("known");
  assert_eq!(code, 1000);
  let (code, _) = unsupported_command("GET_GUILDS").expect("known");
  assert_eq!(code, 1000);
  // Handled elsewhere or genuinely unknown: no entry.
  assert!(unsupported_command("SET_ACTIVITY").is_none());
  assert!(unsupported_command("GET_USER").is_none());
  assert!(unsupported_command("FROBNICATE").is_none());
}

#[test]
fn rich_activity_roundtrips_every_field_through_cached_activity() {
  // Full-rich SET_ACTIVITY as a real game SDK sends it (details, state,
  // timestamps, assets, party, secrets, buttons, flags, emoji, display
  // type, plus one unknown future key): the bridge payload must carry
  // every one of them to consumers, byte-equivalent modulo key order.
  let mut cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","nonce":"rich-1","args":{"pid":424242,"activity":{
      "name":"Rook R.E.P.O. Detail","type":0,"details":"Battle Creek",
      "state":"In Competitive Match","timestamps":{"start":1789148307000},
      "assets":{"large_image":"canary-large","large_text":"Canary","small_image":"ptb-small","small_text":"PTB"},
      "party":{"id":"party_aac0ffee","size":[2,4],"privacy":1},
      "secrets":{"join":"L33tJoin","spectate":"L33tSpec","match":"L33tMatch"},
      "buttons":[{"label":"Play","url":"https://example.com/play"}],
      "instance":true,"flags":1,"status_display_type":1,
      "emoji":{"name":" 잠수","id":"123","animated":false},
      "mystery_field_xyz":"must-survive"}}}"#,
  );
  let fp = activity_fingerprint(&mut cmd);
  let payload = cached_activity(cmd, fp).expect("rich activity encodes");
  let body: Value = serde_json::from_str(&payload.json).expect("valid json");
  let activity = &body["activity"];
  assert_eq!(activity["details"], "Battle Creek");
  assert_eq!(activity["state"], "In Competitive Match");
  assert_eq!(activity["timestamps"]["start"], 1789148307000i64);
  assert_eq!(activity["assets"]["large_image"], "canary-large");
  assert_eq!(activity["party"]["size"], serde_json::json!([2, 4]));
  assert_eq!(activity["party"]["privacy"], 1);
  assert_eq!(activity["secrets"]["join"], "L33tJoin");
  assert_eq!(activity["secrets"]["spectate"], "L33tSpec");
  assert_eq!(activity["secrets"]["match"], "L33tMatch");
  assert_eq!(activity["buttons"], serde_json::json!(["Play"]));
  assert_eq!(activity["instance"], true);
  assert_eq!(activity["status_display_type"], 1);
  assert_eq!(activity["mystery_field_xyz"], "must-survive");
  // Same bytes on the MessagePack leg.
  let decoded: Value = rmp_serde::from_slice(&payload.msgpack).expect("valid msgpack");
  assert_eq!(
    decoded["activity"]["party"]["size"],
    serde_json::json!([2, 4])
  );
  assert_eq!(decoded["activity"]["mystery_field_xyz"], "must-survive");
}

#[test]
fn set_activity_response_echoes_activity_intact() {
  // A non-Playing activity: the echo must preserve name/type/details
  // (official echo semantics) instead of rewriting them.
  let mut cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","nonce":"echo-1","args":{"pid":777,"activity":{
      "name":"My Game","type":2,"details":"AFK"}}}"#,
  );
  cmd.application_id = Some(AppId::from("123"));

  let reply: Value =
    serde_json::from_str(&set_activity_response(&mut cmd).expect("response")).expect("valid json");

  assert_eq!(reply["cmd"], "SET_ACTIVITY");
  assert_eq!(reply["data"]["name"], "My Game");
  assert_eq!(reply["data"]["type"], 2);
  assert_eq!(reply["data"]["details"], "AFK");
  assert_eq!(reply["data"]["application_id"], "123");
  assert_eq!(reply["nonce"], "echo-1");
}

#[test]
fn set_activity_response_keeps_extra_and_derived_metadata() {
  // arRPC #141 parity: the echo carries unknown future keys (extra)
  // and derived metadata (button_urls from buttons), not just the
  // known activity fields.
  let mut cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","nonce":"echo-2","args":{"pid":777,"activity":{
      "name":"My Game","type":0,
      "buttons":[{"label":"Play","url":"https://example.com/play"}],
      "mystery_field_xyz":"must-survive"}}}"#,
  );
  cmd.application_id = Some(AppId::from("123"));
  cmd.fix();

  let reply: Value =
    serde_json::from_str(&set_activity_response(&mut cmd).expect("response")).expect("valid json");

  assert_eq!(
    reply["data"]["metadata"]["button_urls"],
    serde_json::json!(["https://example.com/play"])
  );
  assert_eq!(reply["data"]["mystery_field_xyz"], "must-survive");
}

#[test]
fn set_activity_response_clear_carries_null_data() {
  let mut cmd =
    parse_cmd(r#"{"cmd":"SET_ACTIVITY","nonce":"clear-1","args":{"pid":777,"activity":null}}"#);
  cmd.application_id = Some(AppId::from("123"));

  let reply: Value =
    serde_json::from_str(&set_activity_response(&mut cmd).expect("response")).expect("valid json");

  assert_eq!(reply["cmd"], "SET_ACTIVITY");
  assert!(reply["data"].is_null());
  assert_eq!(reply["nonce"], "clear-1");
}

#[test]
fn dedup_drops_exact_duplicate_inside_window() {
  let mut recent = RecentActivities::new(Duration::from_secs(5), 8);
  let t0 = Instant::now();
  let payload = Bytes::from_static(b"{}");

  assert!(!recent.should_drop(Some(&app1()), 1, Some(&payload), t0));
  assert!(recent.should_drop(
    Some(&app1()),
    1,
    Some(&payload),
    t0 + Duration::from_secs(1)
  ));
  // ...but the same bytes from another pid/app are a different slot.
  assert!(!recent.should_drop(
    Some(&app1()),
    2,
    Some(&payload),
    t0 + Duration::from_secs(1)
  ));
  assert!(!recent.should_drop(
    Some(&other()),
    1,
    Some(&payload),
    t0 + Duration::from_secs(1)
  ));
}

#[test]
fn dedup_passes_changes_clears_and_expiry() {
  let mut recent = RecentActivities::new(Duration::from_secs(5), 8);
  let t0 = Instant::now();
  let a = Bytes::from_static(b"a");
  let b = Bytes::from_static(b"b");

  assert!(!recent.should_drop(Some(&app1()), 1, Some(&a), t0));
  // Any changed byte passes and becomes the new reference.
  assert!(!recent.should_drop(Some(&app1()), 1, Some(&b), t0 + Duration::from_secs(1)));
  assert!(recent.should_drop(Some(&app1()), 1, Some(&b), t0 + Duration::from_secs(2)));
  // A clear always passes and re-arms the slot: the pre-clear payload
  // passes again right after it.
  assert!(!recent.should_drop(Some(&app1()), 1, None, t0 + Duration::from_secs(3)));
  assert!(!recent.should_drop(Some(&app1()), 1, Some(&b), t0 + Duration::from_secs(3)));
  // Past the window, even identical bytes pass.
  assert!(!recent.should_drop(Some(&app1()), 1, Some(&b), t0 + Duration::from_secs(9)));
}

#[test]
fn dedup_table_stays_bounded() {
  let mut recent = RecentActivities::new(Duration::from_secs(5), 4);
  let t0 = Instant::now();
  let payload = Bytes::from_static(b"x");

  for pid in 0..20u64 {
    recent.should_drop(Some(&app1()), pid, Some(&payload), t0);
  }

  assert!(recent.len() <= 4);
  assert!(!recent.is_empty());
}

/// The flood guard tracks a fingerprint without copying it: `should_drop`
/// borrows the caller's `Bytes` and the table keeps its own strong
/// reference to the same buffer, so recording a publish adds no
/// allocation on top of the fingerprint itself. A table that re-copied
/// the payload (a second `to_vec`) would fail here.
#[test]
fn dedup_records_the_buffer_it_was_given() {
  use rsrpc_protocol::commands::activity_fingerprint;

  let mut recent = RecentActivities::new(Duration::from_secs(5), 8);
  let t0 = Instant::now();
  let mut cmd = set_activity_cmd();
  let fp = activity_fingerprint(&mut cmd).expect("fingerprint");
  let fp_ptr = fp.as_ptr();

  assert!(!recent.should_drop(Some(&id123()), 42, Some(&fp), t0));

  let stored = recent
    .recorded(Some(&id123()), 42)
    .expect("slot was recorded");
  assert!(
    std::ptr::eq(stored.as_ptr(), fp_ptr),
    "the table must hold the caller's buffer, not a copy of it"
  );
  assert_eq!(stored, &fp);
}

/// The flood guard keys by the app id it was handed, sharing it.
///
/// A `String` key forced one allocation per publish, on the one path whose
/// frequency is set by how often games re-send presence. With `ActivityCmd`
/// carrying `AppId` (refcounted), the key now borrows that refcount: the
/// recording is pointer-identical to the caller's id, so tracking a publish
/// adds no allocation for the key either.
///
/// A table that rebuilt the id (`to_string`, or `AppId::from` from a `&str`)
/// would store a different allocation and fail here.
#[test]
fn dedup_keys_by_the_app_id_it_was_handed() {
  use rsrpc_protocol::commands::activity_fingerprint;
  use rsrpc_types::AppId;

  let mut recent = RecentActivities::new(Duration::from_secs(5), 8);
  let t0 = Instant::now();
  let mut cmd = set_activity_cmd();
  let fp = activity_fingerprint(&mut cmd).expect("fingerprint");
  let app_id = AppId::from("123");

  assert!(!recent.should_drop(Some(&app_id), 42, Some(&fp), t0));

  let stored_key = recent
    .recorded_key(Some(&app_id), 42)
    .expect("slot was recorded");
  assert_eq!(&**stored_key, "123", "the key carries the caller's id");
  assert!(
    std::sync::Arc::ptr_eq(&stored_key.0, &app_id.0),
    "the key must share the caller's app id, not rebuild it"
  );
  // A different pid is a different slot, as before.
  assert!(recent.recorded_key(Some(&app_id), 43).is_none());
}

/// Cloned payloads share one allocation (`Arc` + refcounted encodings).
#[test]
fn cached_builds_share_one_allocation() {
  use rsrpc_protocol::commands::empty_cached;
  use rsrpc_types::SocketId;

  let payload = empty_cached(42, SocketId::from("sock")).expect("fixed shapes build");
  let shared = payload.clone();
  assert!(std::sync::Arc::ptr_eq(&payload, &shared));
  // Deref reaches both encodings without unwrapping.
  assert!(payload.json.contains("\"pid\":42"));
  assert!(!payload.msgpack.is_empty());
  // Cloning shares the refcounted backing, not the bytes.
  assert!(std::ptr::eq(
    payload.json.as_str().as_ptr(),
    shared.json.as_str().as_ptr()
  ));
  assert_eq!(payload.msgpack.as_ptr(), shared.msgpack.as_ptr());
}

/// `is_clear` is set at construction for all three build shapes.
#[test]
fn cached_payload_knows_whether_it_clears() {
  use rsrpc_protocol::commands::{cached_activity, empty_cached};
  use rsrpc_types::SocketId;
  use rsrpc_types::cmd::ActivityCmd;

  // Clear builds always clear, without parsing to find out.
  assert!(
    empty_cached(1, SocketId::from("s"))
      .expect("fixed shapes build")
      .is_clear
  );

  // Activity builds never clear.
  let mut cmd: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "application_id": "app",
    "args": {"pid": 7, "activity": {"name": "G", "type": 0}},
    "nonce": "n",
  }))
  .unwrap();
  let fp = activity_fingerprint(&mut cmd);
  let payload = cached_activity(cmd, fp).expect("builds");
  assert!(!payload.is_clear);

  // Null-activity commands build clears.
  let clear: ActivityCmd = serde_json::from_value(serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "application_id": "app",
    "args": {"pid": 7, "activity": null},
    "nonce": "n",
  }))
  .unwrap();
  let payload = cached_activity(clear, None).expect("builds");
  assert!(payload.is_clear);
}

/// Canonical `SET_ACTIVITY` command fixture (app 123, pid 42).
fn set_activity_cmd() -> ActivityCmd {
  parse_cmd(
    r#"{
      "cmd": "SET_ACTIVITY",
      "application_id": "123",
      "args": {"pid": 42, "activity": {"name": "Game", "type": 0}},
      "nonce": "n1"
    }"#,
  )
}

/// Fingerprints equal the standalone serialization of the fixed activity.
#[test]
fn activity_fingerprint_matches_standalone_serialization() {
  use rsrpc_protocol::commands::activity_fingerprint;

  let mut cmd = set_activity_cmd();
  let fp = activity_fingerprint(&mut cmd).expect("fingerprint");
  // fix() runs inside; the stored activity must serialize identically.
  let activity = cmd
    .args
    .as_ref()
    .and_then(|args| args.activity.as_ref())
    .expect("activity");
  assert_eq!(fp, serde_json::to_vec(activity).expect("serializable"));
}

/// Activity-less commands (clears) fingerprint to `None`.
#[test]
fn activity_fingerprint_is_none_for_clears() {
  use rsrpc_protocol::commands::activity_fingerprint;

  let mut cmd = parse_cmd(r#"{"cmd":"SET_ACTIVITY","args":{"pid":42},"nonce":"n1"}"#);
  assert!(activity_fingerprint(&mut cmd).is_none());
}

/// Built payloads store the exact fingerprint bytes for change detection.
#[test]
fn cached_activity_carries_fingerprint_bytes() {
  use rsrpc_protocol::commands::activity_fingerprint;

  let mut cmd = set_activity_cmd();
  let fp = activity_fingerprint(&mut cmd).expect("fingerprint");
  let cached = cached_activity(cmd, Some(fp.clone())).expect("payload");
  assert_eq!(cached.activity_json, fp);
  assert!(!cached.is_clear);
}

/// The outer `application_id` is stamped into the activity of the built
/// payload, so the card a consumer replays carries the app that published
/// it. Called without `activity_fingerprint` first: that function stamps it
/// too, so skipping it here is what makes this a test of the by-value path
/// in `cached_activity` itself (where the id is moved, not cloned).
#[test]
fn cached_activity_stamps_outer_application_id() {
  let mut cmd = set_activity_cmd();
  cmd.application_id = Some(AppId::from("424242"));
  let payload = cached_activity(cmd, None).expect("payload");

  let body: Value = serde_json::from_str(&payload.json).expect("valid json");
  assert_eq!(body["activity"]["application_id"], "424242");
}

/// The reply is held against the DOM algorithm it replaced: `to_value` +
/// `insert` + `json!`. The envelope is serialized by reference now, so a
/// shape drift (a dropped key, a changed `evt`/`nonce` null, a different
/// `data` for a clear) must fail here rather than in a game client.
#[test]
fn set_activity_response_matches_the_dom_reference() {
  // Rich activity: every optional field set, so the echo covers them all.
  let mut cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","application_id":"123","args":{"pid":42,"activity":{
      "name":"N","type":2,"url":"https://x.test","created_at":1,"session_id":"s",
      "platform":"desktop","supported_platforms":["desktop"],
      "timestamps":{"start":1789148307000},"application_id":"inner",
      "details":"d","details_url":"https://d.test","state":"st",
      "state_url":"https://s.test","sync_id":"sy","instance":true,
      "status_display_type":1,"flags":1,"emoji":{"name":"😀","id":"1","animated":false},
      "party":{"id":"p","size":[2,4],"privacy":1},
      "secrets":{"join":"J","spectate":"S","match":"M"},
      "buttons":[{"label":"L","url":"https://b.test"}],
      "mystery_field_xyz":"must-survive"}},"nonce":"n9"}"#,
  );

  let got: Value =
    serde_json::from_str(&set_activity_response(&mut cmd).expect("reply")).expect("json");

  // Reference: the exact DOM path this replaced.
  let args = cmd.args.as_ref().expect("args");
  let mut data = serde_json::to_value(args.activity.as_ref().expect("activity")).expect("value");
  data.as_object_mut().expect("object").insert(
    "application_id".to_string(),
    cmd
      .application_id
      .as_deref()
      .map(Value::from)
      .unwrap_or(Value::Null),
  );
  let reference = serde_json::json!({
    "cmd": cmd.cmd,
    "data": data,
    "evt": null,
    "nonce": cmd.nonce,
  });

  assert_eq!(got, reference);
  // The outer id overrides the activity's own, not the other way round.
  assert_eq!(got["data"]["application_id"], "123");
  // Unknown keys still ride along.
  assert_eq!(got["data"]["mystery_field_xyz"], "must-survive");
}

/// A clear echoes `data: null` with the same envelope shape.
#[test]
fn set_activity_response_echoes_clear_as_null_data() {
  let mut cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","application_id":"123","args":{"pid":9,"activity":null},"nonce":"c1"}"#,
  );
  let got: Value =
    serde_json::from_str(&set_activity_response(&mut cmd).expect("reply")).expect("json");

  let reference = serde_json::json!({
    "cmd": cmd.cmd,
    "data": null,
    "evt": null,
    "nonce": cmd.nonce,
  });
  assert_eq!(got, reference);
  assert!(got["data"].is_null());
}

/// A `SET_ACTIVITY` command whose outer `application_id` and the
/// activity's own may diverge.
fn echo_cmd(outer: Option<&str>, inner: Option<&str>) -> ActivityCmd {
  let mut activity = serde_json::json!({ "name": "G", "type": 0 });
  if let Some(v) = inner {
    activity["application_id"] = v.into();
  }
  let mut cmd_json = serde_json::json!({
    "cmd": "SET_ACTIVITY",
    "args": { "pid": 1, "activity": activity },
    "nonce": "n1",
  });
  if let Some(v) = outer {
    cmd_json["application_id"] = v.into();
  }
  serde_json::from_value(cmd_json).expect("failed to parse echo command")
}

fn echo_value(outer: Option<&str>, inner: Option<&str>) -> Value {
  let raw = echo_raw(outer, inner);
  serde_json::from_str(&raw).expect("valid json")
}

fn echo_raw(outer: Option<&str>, inner: Option<&str>) -> String {
  let mut cmd = echo_cmd(outer, inner);
  set_activity_response(&mut cmd).expect("response")
}

/// The N8 contract: the echoed `application_id` is the outer command's id,
/// verbatim — `null` when the command carries none — and it overrides
/// whatever the activity itself sent. This single rule is why the activity
/// cannot be echoed by a plain borrow.
#[test]
fn echo_application_id_is_the_outer_command_id_verbatim() {
  for (outer, inner, want) in [
    (Some("outer"), Some("inner"), Some("outer")),
    (Some("outer"), None, Some("outer")),
    (None, Some("inner"), None),
    (None, None, None),
  ] {
    let got = echo_value(outer, inner);
    // Indexing a missing key also yields `null`, so the raw-string check in
    // `echo_carries_exactly_one_application_id_key` is what proves the key
    // is really present here.
    assert_eq!(
      got["data"]["application_id"],
      want.map(Value::from).unwrap_or(Value::Null),
      "outer={outer:?} inner={inner:?}"
    );
  }
}

/// Exactly one `application_id` key per echo, always present. Echoing the
/// activity through `flatten` while *also* emitting the command's id would
/// write the key twice whenever the activity carries one of its own.
#[test]
fn echo_carries_exactly_one_application_id_key() {
  for (outer, inner) in [
    (Some("outer"), Some("inner")),
    (Some("outer"), None),
    (None, Some("inner")),
    (None, None),
  ] {
    let raw = echo_raw(outer, inner);
    assert_eq!(
      raw.matches("\"application_id\"").count(),
      1,
      "outer={outer:?} inner={inner:?} wrote the key more than once: {raw}"
    );
    assert!(
      raw.contains("\"application_id\":"),
      "outer={outer:?} inner={inner:?} dropped the key: {raw}"
    );
  }
}

/// The activity is written straight into the envelope instead of passing
/// through an intermediate `serde_json::Value`: `application_id`, the one
/// key the echo takes from the command rather than the activity, lands
/// where the DTO declares it — last inside `data`. A `Value`-based build
/// would instead sort the keys alphabetically (a `BTreeMap`), putting
/// `application_id` first.
#[test]
fn echo_serializes_the_activity_without_an_intermediate_value() {
  let mut cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","application_id":"123","args":{"pid":42,"activity":{
      "name":"N","type":2,"url":"https://x.test","created_at":1}},"nonce":"n9"}"#,
  );

  let raw = set_activity_response(&mut cmd).expect("reply");

  assert!(
    raw.contains(r#""application_id":"123"},"evt":null"#),
    "expected the command id as the final key of `data` (no DOM), got: {raw}"
  );
}

/// Outer application ids feed the fingerprint (no cross-app aliasing).
#[test]
fn fingerprint_includes_outer_application_id() {
  // The stamp (outer application_id -> activity.application_id) must
  // happen before fingerprinting: two commands differing only in the
  // outer id must not share a fingerprint, or the second publish would
  // be misclassified as unchanged and keep the old application.
  let mut a = set_activity_cmd();
  let mut b = set_activity_cmd();
  b.application_id = Some(AppId::from("999"));
  let fa = activity_fingerprint(&mut a).expect("fingerprint");
  let fb = activity_fingerprint(&mut b).expect("fingerprint");
  assert_ne!(fa, fb);
}

/// C3: the id stamps move the shared id instead of copying it.
///
/// `activity_fingerprint` and `cached_activity` both stamp the outer
/// `application_id` onto the activity so the two encodings agree, and
/// each runs once per publish. With a `String` field every stamp copied
/// the id — half of what this migration removes, and a per-tick cost on
/// the flood-guard path.
///
/// Pointer identity is the assertion: a round-trip through `String`
/// allocates a fresh buffer and fails here, while the refcounted move
/// does not.
#[test]
fn id_stamps_share_the_command_buffer() {
  let mut cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","application_id":"123456789012345678",
        "args":{"pid":42,"activity":{"name":"G","type":0}},"nonce":"n1"}"#,
  );
  let outer_ptr = cmd.application_id.as_ref().expect("outer id").as_ptr();

  // First stamp (flood-guard fingerprint).
  activity_fingerprint(&mut cmd).expect("fingerprint");
  let stamped = cmd
    .args
    .as_ref()
    .expect("args")
    .activity
    .as_ref()
    .expect("activity")
    .application_id
    .as_ref()
    .expect("stamped");
  assert!(
    std::ptr::eq(stamped.as_ptr(), outer_ptr),
    "the fingerprint stamp must move the shared id, never copy it"
  );
}

/// Every reply builder on the borrowed-envelope path must produce exactly
/// the shape the old `json!` envelope produced: the wire format is what
/// strict clients (and the IPC echo) parse, so a dropped or renamed key
/// must fail here rather than in a game.
#[test]
fn borrowed_envelope_matches_the_previous_shape() {
  let cmd = parse_cmd(
    r#"{"cmd":"SET_ACTIVITY","args":{"pid":1,"activity":null},"nonce":"n1","evt":"VOICE_STATE_UPDATE"}"#,
  );

  // subscribe_ack: {cmd, data:{evt}, evt:null, nonce} — echoing the
  // command's own name, not a hardcoded one.
  let got: Value = serde_json::from_str(&subscribe_ack(&cmd)).expect("json");
  assert_eq!(
    got,
    serde_json::json!({
      "cmd": "SET_ACTIVITY",
      "data": {"evt": "VOICE_STATE_UPDATE"},
      "evt": null,
      "nonce": "n1"
    })
  );

  // generic_ack: data is null, evt is null.
  let got: Value = serde_json::from_str(&generic_ack(&cmd)).expect("json");
  assert_eq!(
    got,
    serde_json::json!({
      "cmd": "SET_ACTIVITY",
      "data": null,
      "evt": null,
      "nonce": "n1"
    })
  );

  // A command that named no event still echoes `{"evt":null}`.
  let no_evt = parse_cmd(r#"{"cmd":"DEEP_LINK","args":null,"nonce":"n2"}"#);
  let got: Value = serde_json::from_str(&subscribe_ack(&no_evt)).expect("json");
  assert_eq!(got["data"]["evt"], serde_json::json!(null));

  // rpc_error: fixed error body, evt ERROR.
  let got: Value = serde_json::from_str(&rpc_error(
    "INVITE_BROWSER",
    &Value::String("n3".into()),
    4011,
    "Invalid invite id",
  ))
  .expect("json");
  assert_eq!(
    got,
    serde_json::json!({
      "cmd": "INVITE_BROWSER",
      "data": {"code": 4011, "message": "Invalid invite id"},
      "evt": "ERROR",
      "nonce": "n3"
    })
  );

  // current_user_update: DISPATCH with the user inside and nonce null.
  let got: Value = serde_json::from_str(&current_user_update(&RpcUser::default())).expect("json");
  assert_eq!(got["cmd"], "DISPATCH");
  assert_eq!(got["evt"], "CURRENT_USER_UPDATE");
  assert_eq!(got["nonce"], serde_json::json!(null));
  assert_eq!(got["data"]["username"], "arRPC");
}

/// Fixtures for the flood-guard tests below: ids shared per test rather
/// than rebuilt per call, so a call's key allocation would show up as a
/// pointer mismatch.
fn app1() -> AppId {
  AppId::from("app")
}

fn other() -> AppId {
  AppId::from("other")
}

fn id123() -> AppId {
  AppId::from("123")
}
