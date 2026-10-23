use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// The `SET_USER` ACK carries exactly the arRPC shape, built from a
/// borrowing envelope.
///
/// It used to serialize the identity into a `Value`, box that in a
/// `json!` map and serialize again — three passes over the same struct,
/// one allocation per field each time. The shape test pins what the
/// envelope must produce, so a DTO that drops a key or renames one fails
/// here rather than at a game client.
#[test]
fn a_set_user_ack_keeps_the_arrpc_shape() {
  let user = Arc::new(Mutex::new(RpcUser::default()));
  let (ack, changed) = handle_bridge_control(
    &user,
    r#"{"type":"SET_USER","nonce":"n1","patch":{"username":"web"}}"#,
  )
  .expect("a SET_USER control message answers");

  let body: serde_json::Value = serde_json::from_str(&ack).expect("the ack is JSON");
  assert_eq!(body["type"], "SET_USER_ACK", "the arRPC ack type is kept");
  assert_eq!(body["nonce"], "n1", "the nonce is echoed");
  assert_eq!(body["data"]["success"], true);
  assert_eq!(
    body["data"]["user"]["username"], "web",
    "the patched identity"
  );
  assert_eq!(
    body.as_object().map(|obj| obj.len()),
    Some(3),
    "the ack carries exactly type, nonce and data"
  );

  let changed = changed.expect("the patch changed the identity");
  assert_eq!(changed.username, "web");
}

/// A control message that changes nothing still answers, and reports no
/// new identity — that is what keeps a repeat `SET_USER` quiet instead
/// of re-fanning-out an identical update.
#[test]
fn an_unchanged_set_user_reports_no_new_identity() {
  let user = Arc::new(Mutex::new(RpcUser::default()));
  let (ack, changed) = handle_bridge_control(
    &user,
    r#"{"type":"SET_USER","nonce":"n2","patch":{"username":"arRPC"}}"#,
  )
  .expect("a SET_USER control message answers");

  let body: serde_json::Value = serde_json::from_str(&ack).expect("the ack is JSON");
  assert_eq!(body["type"], "SET_USER_ACK");
  assert!(
    changed.is_none(),
    "nothing changed, so nothing is fanned out"
  );
}

/// `RESET_USER` restores the startup identity and reports it.
#[test]
fn a_reset_user_restores_the_startup_identity() {
  let user = Arc::new(Mutex::new(RpcUser::default()));
  let (ack, _) = handle_bridge_control(
    &user,
    r#"{"type":"SET_USER","nonce":"n3","patch":{"username":"web"}}"#,
  )
  .expect("patched");
  let _ = ack;

  let (ack, changed) = handle_bridge_control(&user, r#"{"type":"RESET_USER","nonce":"n4"}"#)
    .expect("a RESET_USER control message answers");
  let body: serde_json::Value = serde_json::from_str(&ack).expect("the ack is JSON");
  assert_eq!(body["type"], "RESET_USER_ACK");
  let changed = changed.expect("the reset changed the identity back");
  assert_eq!(changed.username, "arRPC");
}

/// Anything that is not a bridge control message is left alone: the
/// caller echoes it to the sender untouched.
#[test]
fn a_non_control_message_is_ignored() {
  let user = Arc::new(Mutex::new(RpcUser::default()));
  assert!(handle_bridge_control(&user, r#"{"type":"ACTIVITY"}"#).is_none());
  assert!(handle_bridge_control(&user, "not json").is_none());
}

/// The ACK answers with the same identity the handler hands back.
///
/// The ACK is serialized under one guard and the identity used to be
/// cloned from a second, later lock. A control message landing in
/// between — the other transport's pump, which shares the same identity
/// — changed the user between the two reads, so the sender was told one
/// identity while every consumer received a different
/// `CURRENT_USER_UPDATE`. A single call cannot show that: both reads
/// agree whenever nothing runs in between.
///
/// So the shared identity is patched from another thread while the
/// handler runs, and the two answers are compared on every call. What
/// the test asserts is the contract the fan-out relies on, not the lock
/// count that happens to deliver it.
#[test]
fn the_acked_identity_is_the_one_handed_back() {
  let user = Arc::new(Mutex::new(RpcUser::default()));
  let stop = Arc::new(AtomicBool::new(false));
  let patcher = {
    let user = Arc::clone(&user);
    let stop = Arc::clone(&stop);
    std::thread::spawn(move || {
      let mut flip = false;
      while !stop.load(Ordering::Relaxed) {
        flip = !flip;
        let name = if flip { "patcher-a" } else { "patcher-b" };
        // The lock may be held by the handler; skip the round rather
        // than block it. A skipped round only widens the window the
        // regression needs, so the test stays honest either way.
        if let Ok(mut guard) = user.try_lock() {
          guard.patch(&serde_json::json!({ "username": name }));
        } else {
          std::hint::spin_loop();
        }
      }
    })
  };

  for round in 0..256u16 {
    let (ack, changed) = handle_bridge_control(
      &user,
      &format!(
        r#"{{"type":"SET_USER","nonce":"{round}","patch":{{"username":"handler-{round}"}}}}"#
      ),
    )
    .expect("a SET_USER control message answers");
    let ack: serde_json::Value = serde_json::from_str(&ack).expect("the ack is JSON");
    let acked = ack["data"]["user"]["username"]
      .as_str()
      .expect("the ack carries the username");
    let changed = changed.expect("the handler's own patch changed the identity");
    assert_eq!(
      changed.username, acked,
      "round {round}: the identity handed back must be the one the ACK answered"
    );
  }

  stop.store(true, Ordering::Relaxed);
  patcher.join().expect("the patcher thread joins");
}
