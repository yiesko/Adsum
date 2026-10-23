use super::*;

/// A browser command with args echoes those args under their own keys,
/// in `data`, with `args` and `evt` absent.
///
/// This is the wire contract the borrowing echo replaces a clone-based
/// `ActivityCmd` for, so the shape is pinned rather than assumed: the
/// old form built a `HashMap` out of the args and serialized that, and
/// dropping the map is exactly where a key could go missing.
#[test]
fn a_browser_echo_flattens_args_into_data() {
  let event = ActivityCmd {
    application_id: Some(AppId::from("app-1")),
    cmd: "INVITE_BROWSER".to_string(),
    args: Some(ActivityCmdArgs {
      pid: Some(3),
      activity: None,
      code: Some("abc123".to_string()),
      user_id: None,
    }),
    data: None,
    evt: None,
    nonce: Value::String("n1".to_string()),
  };

  let body: Value =
    serde_json::from_str(&serde_json::to_string(&BrowserEcho::borrow(&event)).unwrap())
      .expect("the echo is JSON");

  assert_eq!(body["cmd"], "INVITE_BROWSER");
  assert_eq!(body["application_id"], "app-1");
  assert_eq!(body["nonce"], "n1");
  assert_eq!(body["data"]["code"], "abc123", "the code lands in data");
  assert_eq!(body["data"]["pid"], 3, "the pid lands in data too");
  assert!(body.get("args").is_none(), "args stays absent");
  assert!(body.get("evt").is_none(), "evt stays absent");
}

/// A command with no args echoes no `data` at all — the omitted key is
/// what the old `HashMap`-based shape produced too.
#[test]
fn a_browser_echo_without_args_has_no_data() {
  let event = ActivityCmd {
    application_id: None,
    cmd: "DEEP_LINK".to_string(),
    args: None,
    data: None,
    evt: None,
    nonce: Value::String("n2".to_string()),
  };

  let body: Value =
    serde_json::from_str(&serde_json::to_string(&BrowserEcho::borrow(&event)).unwrap())
      .expect("the echo is JSON");

  assert_eq!(body["cmd"], "DEEP_LINK");
  assert!(body.get("data").is_none(), "no args, no data");
  assert!(body.get("application_id").is_none(), "absent stays absent");
}

/// Args with `None` fields omit those keys, exactly as the old
/// `HashMap`-from-`to_value` shape did — the map dropped absent keys,
/// and so does the borrowed echo struct.
#[test]
fn a_browser_echo_omits_absent_arg_keys() {
  let event = ActivityCmd {
    application_id: Some(AppId::from("app-1")),
    cmd: "GIFT_CODE_BROWSER".to_string(),
    args: Some(ActivityCmdArgs {
      pid: None,
      activity: None,
      code: Some("xyz".to_string()),
      user_id: None,
    }),
    data: None,
    evt: None,
    nonce: Value::String("n3".to_string()),
  };

  let body: Value =
    serde_json::from_str(&serde_json::to_string(&BrowserEcho::borrow(&event)).unwrap())
      .expect("the echo is JSON");

  let data = body["data"].as_object().expect("data is an object");
  assert!(data.contains_key("code"), "the present key survives");
  assert!(!data.contains_key("pid"), "an absent key stays absent");
  assert!(!data.contains_key("activity"), "an absent key stays absent");
  assert!(!data.contains_key("user_id"), "an absent key stays absent");
}

/// A browser echo keeps the legacy `activity` shape: an explicit null is
/// omitted from `data`, while a real activity still travels as an object.
#[test]
fn a_browser_echo_keeps_the_legacy_activity_shape() {
  let with_null: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"INVITE_BROWSER","args":{"pid":3,"activity":null,"code":"abc"},"nonce":"n5"}"#,
  )
  .expect("the command parses");
  let body: Value =
    serde_json::from_str(&serde_json::to_string(&BrowserEcho::borrow(&with_null)).unwrap())
      .expect("the echo is JSON");
  let data = body["data"].as_object().expect("data is an object");
  assert!(
    !data.contains_key("activity"),
    "an explicit null stays omitted"
  );
  assert_eq!(data["code"], "abc");

  let with_activity: ActivityCmd = serde_json::from_str(
    r#"{"cmd":"INVITE_BROWSER","args":{"pid":3,"activity":{"name":"G","type":0},"code":"abc"},"nonce":"n6"}"#,
  )
  .expect("the command parses");
  let body: Value =
    serde_json::from_str(&serde_json::to_string(&BrowserEcho::borrow(&with_activity)).unwrap())
      .expect("the echo is JSON");
  assert_eq!(
    body["data"]["activity"]["name"], "G",
    "a real activity still travels as an object"
  );
}

/// A `GET_USER` reply carries the borrowed user, id included.
///
/// Pins the shape this reply had while it was built from a clone: the
/// serialization is unchanged, so only where the value came from moved.
#[test]
fn a_get_user_reply_carries_the_borrowed_user() {
  let event = get_user_cmd(None);

  let body: Value =
    serde_json::from_str(&get_user_response(&event, &fixture_user())).expect("the reply is JSON");

  assert_eq!(body["cmd"], "GET_USER");
  assert_eq!(body["data"]["id"], "42", "the id travels");
  assert_eq!(body["data"]["username"], "tester");
}

/// A `GET_USER` for someone else answers null data, not an empty user.
#[test]
fn a_get_user_for_a_stranger_answers_null() {
  let event = get_user_cmd(Some("99".to_string()));

  let body: Value =
    serde_json::from_str(&get_user_response(&event, &fixture_user())).expect("the reply is JSON");

  assert_eq!(body["cmd"], "GET_USER");
  assert!(body["data"].is_null(), "a stranger gets null data");
}

/// The matching user gets the user, so the id check is symmetric: asking
/// for yourself is not a stranger.
#[test]
fn a_get_user_for_yourself_answers_the_user() {
  let event = get_user_cmd(Some("42".to_string()));

  let body: Value =
    serde_json::from_str(&get_user_response(&event, &fixture_user())).expect("the reply is JSON");

  assert_eq!(body["data"]["id"], "42", "asking for yourself matches");
}

fn fixture_user() -> RpcUser {
  RpcUser {
    id: "42".to_string(),
    username: "tester".to_string(),
    discriminator: "0001".to_string(),
    global_name: Some("Tester".to_string()),
    avatar: None,
    avatar_decoration_data: None,
    bot: false,
    flags: 0,
    premium_type: 0,
  }
}

fn get_user_cmd(user_id: Option<String>) -> ActivityCmd {
  ActivityCmd {
    application_id: None,
    cmd: "GET_USER".to_string(),
    args: Some(ActivityCmdArgs {
      pid: None,
      activity: None,
      code: None,
      user_id,
    }),
    data: None,
    evt: None,
    nonce: Value::String("n4".to_string()),
  }
}
