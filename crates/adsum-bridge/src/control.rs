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
#[path = "../tests/unit/control.rs"]
mod tests;
