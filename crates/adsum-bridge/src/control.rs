//! Bridge control messages: `SET_USER` / `RESET_USER` (arRPC parity).
//!
//! Any localhost consumer may send them; the handler answers with an ACK
//! and reports the new identity when it changed. Callers echo anything
//! else back to the sender untouched.

use std::sync::{Arc, Mutex};

use adsum_types::user::RpcUser;
use serde::Serialize;

/// The `SET_USER`/`RESET_USER` ACK envelope, borrowing the identity.
///
/// The shape used to be produced by serializing the identity into a
/// `Value`, boxing that in a `json!` map and serializing the whole thing
/// again: three passes over the same struct, one allocation per field each
/// time. The ACK and the fanned-out update need the same identity, so it is
/// serialized once from here and nowhere else.
#[derive(Serialize)]
struct UserAck<'a> {
  #[serde(rename = "type")]
  ack_type: &'static str,
  nonce: &'a serde_json::Value,
  data: UserAckData<'a>,
}

/// The `data` object of a user ACK: the arRPC success flag plus the
/// identity itself, borrowed rather than copied into a `Value`.
#[derive(Serialize)]
struct UserAckData<'a> {
  success: bool,
  user: &'a RpcUser,
}

/// Handle a bridge control message (`SET_USER`/`RESET_USER`, arRPC parity).
/// Returns the ACK text plus the new identity when it changed, `None` for
/// anything else (the caller echoes those to the sender untouched).
pub(crate) fn handle_bridge_control(
  user: &Arc<Mutex<RpcUser>>,
  text: &str,
) -> Option<(String, Option<RpcUser>)> {
  let body: serde_json::Value = serde_json::from_str(text).ok()?;
  let msg_type = body.get("type")?.as_str()?;
  if !matches!(msg_type, "SET_USER" | "RESET_USER") {
    return None;
  }
  let nonce = body
    .get("nonce")
    .cloned()
    .unwrap_or(serde_json::Value::Null);
  // One critical section for the whole read-modify-read: patch/reset and
  // both snapshots share a single guard. The identity is cloned once for
  // the caller (the fan-out update) and compared in place against the
  // pre-patch snapshot, instead of being cloned twice and compared after
  // the guard is gone.
  let (ack, identity) = {
    let mut guard = user.lock().unwrap_or_else(|e| e.into_inner());
    let before = guard.clone();
    if msg_type == "SET_USER" {
      // `patch` (arRPC shape) or `data` (defensive alias) carry the patch.
      if let Some(patch) = body.get("patch").or_else(|| body.get("data")) {
        guard.patch(patch);
      }
    } else {
      // Reset to the startup identity (defaults + `ADSUM_USER_*`).
      *guard = RpcUser::from_env();
    }
    let ack_type = if msg_type == "SET_USER" {
      "SET_USER_ACK"
    } else {
      "RESET_USER_ACK"
    };
    // Serialize while the guard is held: the ACK is a pure read of state we
    // already own, and holding it costs nothing measurable for a message
    // that arrives at most a few times per session.
    let ack = serde_json::to_string(&UserAck {
      ack_type,
      nonce: &nonce,
      data: UserAckData {
        success: true,
        user: &guard,
      },
    })
    .ok()?;
    // Only a real change is handed back: the bridge fans out
    // CURRENT_USER_UPDATE from it, and a repeat SET_USER with the same patch
    // must stay quiet. Cloned from the guard the ACK was just serialized
    // from, so the two cannot describe different identities: a control
    // message on the other pump landing between here and a second lock
    // would hand back an identity the ACK never mentioned.
    let identity = (*guard != before).then(|| guard.clone());
    (ack, identity)
  };
  Some((ack, identity))
}

#[cfg(test)]
mod tests {
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
}
