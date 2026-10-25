//! Consumer management: who listens on each encoding, fan-out delivery,
//! dead-client pruning.
//!
//! Consumers connect on the JSON or MessagePack port (overridable per
//! consumer via `format=`); every publish fans out to both tables while
//! pruning dead responders, so a stuck client cannot pin memory (its
//! queued frames) forever.

use std::sync::Mutex;
use std::sync::atomic::Ordering;

use adsum_protocol::commands::CachedActivity;
use adsum_protocol::query::query_param;
use adsum_types::cmd::ActivityCmd;
use adsum_ws::{ClientId, Message, Responder, TextBytes};
use bytes::Bytes;
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
    match query_param(uri, "format") {
      Some("msgpack") | Some("messagepack") => BridgeProtocol::MsgPack,
      Some("json") => BridgeProtocol::Json,
      _ => default,
    }
  }
}

/// One outbound frame, encoded once and shared by every consumer it is
/// sent to.
///
/// The JSON text is what the producers already hold. MessagePack is
/// derived from it on first use and cached, so a broadcast to K
/// MessagePack consumers pays one parse and one encode instead of K —
/// which is what re-encoding per consumer used to cost on a path that
/// sends the same bytes to all of them.
pub(crate) struct Frame {
  json: TextBytes,
  msgpack_cell: std::sync::OnceLock<Option<Bytes>>,
  /// How many times the MessagePack form was derived. Test-only: the
  /// `OnceLock` already makes "at most one" structural, and this makes it
  /// *observable* so a regression that derives eagerly (or per consumer)
  /// fails a test instead of passing silently.
  #[cfg(test)]
  derivations: std::sync::atomic::AtomicUsize,
}

impl Frame {
  /// Borrow a JSON string as a frame. No encoding happens here.
  pub(crate) fn new(json: impl Into<TextBytes>) -> Self {
    Self {
      json: json.into(),
      msgpack_cell: std::sync::OnceLock::new(),
      #[cfg(test)]
      derivations: std::sync::atomic::AtomicUsize::new(0),
    }
  }

  /// The MessagePack form, derived once and cached (success or failure).
  fn msgpack(&self) -> Option<&Bytes> {
    let cell = &self.msgpack_cell;
    let derived = cell.get_or_init(|| {
      #[cfg(test)]
      self
        .derivations
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
      encode_msgpack(&self.json)
    });
    derived.as_ref()
  }

  /// Derivations so far (test-only).
  #[cfg(test)]
  fn derivations(&self) -> usize {
    self.derivations.load(std::sync::atomic::Ordering::Relaxed)
  }
}

/// Derive the MessagePack form from a JSON text: one parse, one encode.
///
/// A failure is cached too (as `None`) so a malformed frame is not
/// re-parsed for every consumer — the producer's own serializer makes
/// that unreachable in practice, but the fan-out should not depend on it.
fn encode_msgpack(json: &str) -> Option<Bytes> {
  let value: serde_json::Value = serde_json::from_str(json).ok()?;
  rmp_serde::to_vec_named(&value).ok().map(Bytes::from)
}

/// Send one pre-encoded frame to a consumer, in the encoding it speaks.
///
/// The MessagePack form is derived from the frame's JSON on the first
/// MessagePack consumer and reused by the rest, so a fan-out encodes once.
/// Returns whether the frame was queued.
pub(crate) fn send_frame(responder: &Responder, frame: &Frame, protocol: BridgeProtocol) -> bool {
  match protocol {
    BridgeProtocol::Json => responder
      .try_send(Message::Text(frame.json.clone()))
      .is_ok(),
    BridgeProtocol::MsgPack => frame
      .msgpack()
      .and_then(|bytes| responder.try_send(Message::Binary(bytes.clone())).ok())
      .is_some(),
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
        cmd.application_id.as_ref(),
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
    // speaks it (not "is this map empty"). One pass answers both questions
    // (the old two `any()` scans walked the same table twice).
    let mut wants_json = false;
    let mut wants_msgpack = false;
    for consumer in consumers.values() {
      match consumer.protocol {
        BridgeProtocol::Json => wants_json = true,
        BridgeProtocol::MsgPack => wants_msgpack = true,
      }
    }
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

#[cfg(test)]
#[path = "../tests/unit/consumer.rs"]
mod tests;
