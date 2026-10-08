//! Consumer management: who listens on each encoding, fan-out delivery,
//! dead-client pruning.
//!
//! Consumers connect on the JSON or MessagePack port (overridable per
//! consumer via `format=`); every publish fans out to both tables while
//! pruning dead responders, so a stuck client cannot pin memory (its
//! queued frames) forever.

use std::sync::Mutex;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use rsrpc_protocol::commands::CachedActivity;
use rsrpc_protocol::query::query_params;
use rsrpc_types::cmd::ActivityCmd;
use rsrpc_ws::{ClientId, Message, Responder, TextBytes};
use rustc_hash::FxHashMap;

use super::bridge::Shared;

/// Which wire encoding a bridge consumer speaks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum BridgeProtocol {
  /// JSON text frames (port 1337, the arRPC-compatible bridge).
  Json,
  /// MessagePack binary frames (port 1338).
  MsgPack,
}

impl BridgeProtocol {
  /// The protocol requested by the consumer's `format=` query parameter,
  /// falling back to the per-port default when absent.
  pub(crate) fn from_query(uri: &str, default: BridgeProtocol) -> BridgeProtocol {
    match query_params(uri).get("format").map(String::as_str) {
      Some("msgpack") | Some("messagepack") => BridgeProtocol::MsgPack,
      Some("json") => BridgeProtocol::Json,
      _ => default,
    }
  }
}

/// Send a JSON string, encoding it to MessagePack when the consumer speaks
/// MessagePack. Returns whether the frame was queued.
pub(crate) fn send_message(responder: &Responder, data: &str, protocol: BridgeProtocol) -> bool {
  match protocol {
    BridgeProtocol::Json => responder
      .try_send(Message::Text(data.to_string().into()))
      .is_ok(),
    BridgeProtocol::MsgPack => {
      if let Ok(value) = serde_json::from_str::<serde_json::Value>(data)
        && let Ok(bytes) = rmp_serde::to_vec_named(&value)
      {
        responder
          .try_send(Message::Binary(bytes::Bytes::from(bytes)))
          .is_ok()
      } else {
        false
      }
    }
  }
}

/// Send an already dual-encoded activity payload to a consumer.
pub(crate) fn send_cached(
  responder: &Responder,
  payload: &CachedActivity,
  protocol: BridgeProtocol,
) {
  match protocol {
    BridgeProtocol::Json => {
      let _ = responder.try_send(Message::Text(payload.json.clone()));
    }
    BridgeProtocol::MsgPack => {
      let _ = responder.try_send(Message::Binary(payload.msgpack.clone()));
    }
  }
}

/// One registered consumer: its responder plus the encoding it speaks.
///
/// These three facts used to live in three maps keyed by the same id, so
/// every message took three locks to answer "who is this and what do they
/// speak", and a prune had to touch two more maps or leave protocol entries
/// pinned forever. One map means one lock.
#[derive(Clone, Debug)]
pub(crate) struct Consumer {
  pub(crate) responder: Responder,
  pub(crate) protocol: BridgeProtocol,
}

/// Locked view of the consumer map: one guard for everything a message
/// needs.
pub(crate) type ConsumerMap = Mutex<FxHashMap<ClientId, Consumer>>;

impl Shared {
  /// Whether this id completed a Connect registration. Refused origins
  /// are closed without registering, but a pipelined frame can still
  /// arrive before the close lands — and the transport may deliver it —
  /// so control paths must check this instead of trusting arrival order.
  pub(crate) fn is_registered(&self, id: ClientId) -> bool {
    self
      .consumers
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .contains_key(&id)
  }

  /// Encoding a consumer speaks: the resolved `format=` override recorded
  /// at connect, falling back to the port default for unknown ids (which
  /// cannot happen: every insert is paired with an entry).
  pub(crate) fn protocol_for(&self, id: ClientId, default: BridgeProtocol) -> BridgeProtocol {
    self
      .consumers
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .get(&id)
      .map(|consumer| consumer.protocol)
      .unwrap_or(default)
  }

  /// Flood guard: byte-identical republishes inside the window are
  /// dropped — before any broadcast, cache write or log line.
  /// Takes the precomputed fingerprint so dropped publishes never pay
  /// for the envelope build. Borrows it: the flood guard records a
  /// strong reference to the same buffer rather than a copy.
  pub(crate) fn flood_dropped(&self, cmd: &ActivityCmd, fingerprint: Option<&Bytes>) -> bool {
    let args = cmd.args.as_ref();
    let pid = args.and_then(|args| args.pid).unwrap_or_default();
    self
      .recent
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .should_drop(
        cmd.application_id.as_deref().unwrap_or(""),
        pid,
        fingerprint,
        std::time::Instant::now(),
      )
  }

  /// Send one payload to every connected consumer, pruning dead ones so a
  /// stuck client cannot pin memory (its queued frames) forever.
  pub(crate) fn send_to_all(&self, payload: &CachedActivity) {
    let mut consumers = self.consumers.lock().unwrap_or_else(|e| e.into_inner());
    if consumers.is_empty() {
      tracing::debug!("[bridge] No consumers connected, skipping");
      return;
    }
    // try_send never blocks: a full outbox reports Full here and the slot is
    // released now, not pinned until a lagging Disconnect. The failed send
    // is what marks the consumer dead.
    let dead: Vec<ClientId> = consumers
      .iter()
      .filter_map(|(id, consumer)| {
        let message = match consumer.protocol {
          BridgeProtocol::Json => Message::Text(payload.json.clone()),
          BridgeProtocol::MsgPack => Message::Binary(payload.msgpack.clone()),
        };
        consumer.responder.try_send(message).err().map(|_| *id)
      })
      .collect();
    self
      .dropped_broadcasts
      .fetch_add(dead.len() as u64, Ordering::Relaxed);
    for id in dead {
      tracing::warn!("[bridge] Pruning dead consumer {id}");
      consumers.remove(&id);
    }
  }

  /// Broadcast a non-activity event as-is, serializing once per encoding.
  /// A frame that cannot encode is dropped loudly (not silently).
  ///
  /// Each encoding is serialized exactly once and handed to a refcounted
  /// payload, so the per-consumer clone is an O(1) refcount bump instead of
  /// a deep copy of the whole frame (`mem-zero-copy`).
  pub(crate) fn broadcast_raw(&self, cmd: &ActivityCmd) {
    let mut consumers = self.consumers.lock().unwrap_or_else(|e| e.into_inner());
    if consumers.is_empty() {
      tracing::debug!("[bridge] No consumers connected, skipping");
      return;
    }
    // Serialized once per encoding, and only when a consumer actually
    // speaks it: the check is "does anyone speak this" rather than "is this
    // map empty", which is what one map makes natural.
    let wants_json = consumers
      .values()
      .any(|c| c.protocol == BridgeProtocol::Json);
    let wants_msgpack = consumers
      .values()
      .any(|c| c.protocol == BridgeProtocol::MsgPack);
    let json_payload: Option<TextBytes> = if wants_json {
      match serde_json::to_string(cmd) {
        Ok(payload) => Some(TextBytes::from(payload)),
        Err(err) => {
          tracing::debug!("[bridge] Dropping unserializable fan-out frame: {err}");
          None
        }
      }
    } else {
      None
    };
    let msgpack_payload: Option<Bytes> = if wants_msgpack {
      match rmp_serde::to_vec_named(cmd) {
        Ok(payload) => Some(Bytes::from(payload)),
        Err(err) => {
          tracing::debug!("[bridge] Dropping unserializable fan-out frame: {err}");
          None
        }
      }
    } else {
      None
    };
    for (protocol, payload) in [
      (BridgeProtocol::Json, json_payload.map(Message::Text)),
      (
        BridgeProtocol::MsgPack,
        msgpack_payload.map(Message::Binary),
      ),
    ] {
      let Some(payload) = payload else { continue };
      let dead: Vec<ClientId> = consumers
        .iter()
        .filter(|(_, consumer)| consumer.protocol == protocol)
        .filter_map(|(id, consumer)| {
          consumer
            .responder
            .try_send(payload.clone())
            .err()
            .map(|_| *id)
        })
        .collect();
      self
        .dropped_broadcasts
        .fetch_add(dead.len() as u64, Ordering::Relaxed);
      for id in dead {
        tracing::warn!("[bridge] Pruning dead consumer {id}");
        consumers.remove(&id);
      }
    }
  }
}
