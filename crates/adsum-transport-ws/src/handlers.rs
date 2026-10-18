//! Per-command handlers: protocol replies plus sink forwarding.
//!
//! Every handler returns whether the client is still alive; `false` prunes
//! the slot immediately instead of waiting for a `Disconnect` that may lag
//! behind under flood. Replies go through the private `reply` helper, a
//! bounded wait: the shared pump must never park behind one reader that
//! stopped draining.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use adsum_protocol::commands;
use adsum_types::AppId;
use adsum_types::cmd::{ActivityCmd, ActivityCmdArgs};
use adsum_types::user::RpcUser;
use adsum_ws::{Message, Responder};
use serde::Serialize;
use serde_json::Value;

use crate::transport::{PendingQueue, Sink};

/// Upper bound a handler waits for outbox space before treating the client
/// as stalled (pruned, freeing the pump for everyone else).
const REPLY_SEND_TIMEOUT: Duration = Duration::from_millis(250);

/// Queue one reply with a bounded wait. `false` means the client is gone
/// or stalled past the budget: the caller prunes it.
pub(crate) async fn reply(responder: &Responder, message: Message) -> bool {
  responder
    .send_timeout(message, REPLY_SEND_TIMEOUT)
    .await
    .is_ok()
}

/// Serialize the `GET_USER` reply from a **borrowed** user.
///
/// Synchronous on purpose: the caller holds the shared user's guard only
/// for this call and releases it before awaiting the reply — a std `Mutex`
/// must never be held across a suspension point. Borrowing also means the
/// reply is serialized straight from the shared value: the snapshot this
/// replaces deep-copied every field of `RpcUser` on every `GET_USER`.
pub(crate) fn get_user_response(event: &ActivityCmd, user: &RpcUser) -> String {
  let wanted = event.args.as_ref().and_then(|args| args.user_id.as_ref());
  let matched = wanted.is_none_or(|id| *id == user.id);
  commands::user_response(event, matched.then_some(user))
}

/// Echo for a browser command (invite/template/gift): the command's own
/// identity with its `args` echoed under `data`.
///
/// Borrows instead of cloning, and writes the args straight into the
/// `data` key rather than routing them through a `serde_json::Value`
/// object, a `HashMap` rebuild and a second serialization: the old shape
/// serialized the args once to build that map and again to write the
/// reply, and copied `application_id`, `cmd` and `nonce` to do it.
#[derive(Serialize)]
struct BrowserEcho<'a> {
  #[serde(skip_serializing_if = "Option::is_none")]
  application_id: Option<&'a AppId>,
  cmd: &'a str,
  /// The command's args, nested under `data` exactly where the wire
  /// contract puts them (`args`/`evt` stay absent). Written through the
  /// struct serializer: no intermediate map, no re-serialization.
  #[serde(
    serialize_with = "serialize_args_as_data",
    skip_serializing_if = "Option::is_none"
  )]
  data: Option<&'a ActivityCmdArgs>,
  nonce: &'a Value,
}

/// Serialize args into the `data` value, borrowing the struct.
fn serialize_args_as_data<S: serde::Serializer>(
  args: &Option<&ActivityCmdArgs>,
  serializer: S,
) -> Result<S::Ok, S::Error> {
  match args {
    Some(args) => args.serialize(serializer),
    None => serializer.serialize_none(),
  }
}

impl<'a> BrowserEcho<'a> {
  fn borrow(event: &'a ActivityCmd) -> Self {
    Self {
      application_id: event.application_id.as_ref(),
      cmd: &event.cmd,
      data: event.args.as_ref(),
      nonce: &event.nonce,
    }
  }
}

/// Forward an invite/template/gift command after validating its code.
pub(crate) async fn handle_browser_command(
  event: ActivityCmd,
  responder: &Responder,
  sink: &Sink,
) -> bool {
  // Discord error codes for unusable invite/template/gift ids.
  let (code, message) = if event.cmd == "GUILD_TEMPLATE_BROWSER" {
    (4017_u16, "Invalid guild template id")
  } else if event.cmd == "GIFT_CODE_BROWSER" {
    (4016_u16, "Invalid gift code")
  } else {
    (4011_u16, "Invalid invite id")
  };
  let has_code = event
    .args
    .as_ref()
    .and_then(|args| args.code.as_ref())
    .is_some_and(|code| !code.trim().is_empty());
  if !has_code {
    tracing::warn!("[transport-ws] {} without code", event.cmd);
    return reply(
      responder,
      Message::Text(commands::rpc_error(&event.cmd, &event.nonce, code, message).into()),
    )
    .await;
  }

  // Optimistic forward; the outcome lives downstream. The echo is built by
  // borrowing, so the command itself is the only thing that moves — the
  // old path deep-cloned the whole body just to send it and then copied
  // three fields to answer.
  let echo = serde_json::to_string(&BrowserEcho::borrow(&event));
  sink.emit(event).await;

  // Client-supplied `data` may hold non-finite floats, which JSON cannot
  // encode: drop loudly instead of ending the pump. The forward above
  // already left, so the client is still considered alive.
  let Ok(response) = echo else {
    tracing::warn!("[transport-ws] Dropping unserializable response for a browser command");
    return true;
  };
  reply(responder, Message::Text(response.into())).await
}

/// Acknowledge a deep link (forwarded downstream).
pub(crate) async fn handle_deep_link(event: &ActivityCmd, responder: &Responder) -> bool {
  let response = ActivityCmd {
    application_id: event.application_id.clone(),
    cmd: event.cmd.clone(),
    args: None,
    data: None,
    evt: None,
    nonce: event.nonce.clone(),
  };
  let Ok(response) = serde_json::to_string(&response) else {
    tracing::warn!("[transport-ws] Dropping unserializable deep-link response");
    return true;
  };
  reply(responder, Message::Text(response.into())).await
}

/// Answer `CONNECTIONS_CALLBACK` with the official-shaped error.
pub(crate) async fn handle_connections_callback(
  event: &ActivityCmd,
  responder: &Responder,
) -> bool {
  let mut data = HashMap::new();
  data.insert("code".to_string(), Value::Number(1000.into()));
  let response = ActivityCmd {
    application_id: event.application_id.clone(),
    cmd: event.cmd.clone(),
    args: None,
    data: Some(data),
    evt: Some("ERROR".to_string()),
    nonce: event.nonce.clone(),
  };
  // `data` is locally built (no client floats): encode failure would be a
  // coding bug, but the pump must not die on it.
  let Ok(response) = serde_json::to_string(&response) else {
    tracing::warn!("[transport-ws] Dropping unserializable connections response");
    return true;
  };
  reply(responder, Message::Text(response.into())).await
}

/// Blind-ACK a subscription (no voice/guild backend exists; clients wait
/// for the lock-step reply).
pub(crate) async fn handle_subscribe(event: &ActivityCmd, responder: &Responder) -> bool {
  reply(
    responder,
    Message::Text(commands::subscribe_ack(event).into()),
  )
  .await
}

/// Answer unknown commands from the shared unbacked-command table.
pub(crate) async fn handle_unknown(cmd: &str, event: &ActivityCmd, responder: &Responder) -> bool {
  let unsupported = commands::unsupported_command(cmd);
  if unsupported.is_none() {
    tracing::warn!("[transport-ws] Unknown command: {cmd}");
  }
  let (code, message) = unsupported.unwrap_or((1000, "Unknown command"));
  reply(
    responder,
    Message::Text(commands::rpc_error(&event.cmd, &event.nonce, code, message).into()),
  )
  .await
}

/// How a `SET_ACTIVITY` command reaches the sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Forward {
  /// Normal path: bounded-wait enqueue straight into the sink.
  Direct,
  /// Wedge recovery: queue behind older staged clears (never waits, never
  /// overtakes them). Used when the staged queue is still non-empty after
  /// the per-message flush, i.e. the sink is still full.
  Staged,
  /// Refused: reply only, never forwarded (over the tracking bound, or a
  /// pruned slot). Lock-step clients must receive *some* reply or hang.
  Refused,
}

/// Forward `SET_ACTIVITY` and confirm with the arRPC-shaped reply.
///
/// Returns `(alive, delivered)`: `alive` prunes the slot when `false`;
/// `delivered` tells the caller whether the card reached (or is queued
/// behind older clears toward) the sink, so shed publishes stay untracked
/// (nothing shown, nothing to clear). The fixed command (with the
/// connect-query `client_id` fallback applied) goes to the sink; the
/// caller records the slim [`PublishedSlot`] itself, so no second deep
/// clone happens here.
pub(crate) async fn handle_set_activity(
  mut event: ActivityCmd,
  query_client_id: Option<&str>,
  responder: &Responder,
  sink: &Sink,
  pending_clears: &Mutex<PendingQueue>,
  forward: Forward,
) -> (bool, bool) {
  // Fall back to the client_id provided on connect (query param) when the
  // command itself does not carry an application_id.
  if event.application_id.is_none() {
    event.application_id = query_client_id.map(AppId::from);
  }

  // Apply field fixes so the confirmation reply carries labels/urls (fix is
  // idempotent, so a downstream fix pass is harmless).
  event.fix();
  // Materialize the reply *before* the command moves into the sink: the
  // ACK is the only part of the command still needed afterwards, and the
  // command was just parsed so nothing downstream ever saw a shorter life.
  let response = commands::set_activity_response(&mut event);
  let delivered = match forward {
    Forward::Direct => sink.emit(event).await,
    // Staged publishes stay tracked: the disconnect path flushes them
    // before its own clears, so a never-delivered publish degrades to a
    // harmless clear of an unshown card (ignored downstream).
    Forward::Staged => {
      // Try the sink directly and only queue on a full one: staging and
      // immediately draining cost two lock round-trips for every staged
      // publish, including the ones that left the queue at once.
      sink.emit_or_stage_publish(pending_clears, event)
    }
    Forward::Refused => false,
  };

  // Confirm to the game client; some RPC libraries wait for this before
  // considering the presence set. No confirm to send means the client is
  // still considered alive.
  let alive = match response {
    Some(response) => reply(responder, Message::Text(response.into())).await,
    None => true,
  };
  (alive, delivered)
}

/// Slim per-pid publication record: everything a disconnect clear needs,
/// without retaining the full command. A connection normally publishes
/// one pid; companions multiplexing several stay covered up to
/// [`MAX_TRACKED_PIDS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedSlot {
  /// Refcounted like the command's own id: this is cloned out per publish
  /// and per disconnect clear, and it used to be a `String` copy each time.
  pub app_id: Option<AppId>,
  pub pid: u64,
  pub nonce: Value,
}

/// Cap on tracked pids per connection: bounds memory against pathological
/// publishers while covering every realistic multiplexer. Beyond the cap
/// new pids are refused before forwarding (see `note_published`), so every
/// forwarded card stays tracked and disconnect cleanup stays complete —
/// nothing ghosts, nothing is ever cleared while live.
pub(crate) const MAX_TRACKED_PIDS: usize = 16;

/// Build the clear command emitted when a published pid dies.
pub(crate) fn clear_for_slot(published: &PublishedSlot) -> ActivityCmd {
  ActivityCmd {
    application_id: published.app_id.clone(),
    cmd: "SET_ACTIVITY".to_string(),
    data: None,
    evt: None,
    args: Some(ActivityCmdArgs {
      // pid 0 is never a genuine clear, so it is safely ignored
      // downstream — same convention as the old whole-command clear.
      pid: Some(published.pid),
      activity: None,
      code: None,
      user_id: None,
    }),
    nonce: published.nonce.clone(),
  }
}

#[cfg(test)]
mod tests {
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
  /// and so does `skip_serializing_none` on the args struct.
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
}
