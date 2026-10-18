use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use serde_json::Value;
use serde_with::skip_serializing_none;
use tungstenite::Utf8Bytes;

use adsum_types::cmd::{Activity, ActivityCmd, ActivityPayload};
use adsum_types::user::RpcUser;
use adsum_types::{AppId, SocketId};

#[skip_serializing_none]
#[derive(Serialize)]
pub struct ProcessActivity {
  pub application_id: AppId,
  /// Refcounted like `ScannedGame::name`: the payload builder clones it
  /// per publish, and a `String` there copied the game name every time.
  pub name: Arc<str>,
  pub timestamps: ProcessTimestamps,
  pub r#type: u32,
  pub metadata: HashMap<String, String>,
  pub flags: u32,
}

#[derive(Serialize)]
pub struct ProcessTimestamps {
  pub start: u64,
}

#[derive(Serialize)]
pub struct ProcessPayload {
  pub activity: ProcessActivity,
  pub pid: u64,
  #[serde(rename = "socketId")]
  pub socket_id: SocketId,
}

/// An activity payload serialized once for both bridge protocols (JSON text
/// frames for the 1337 port, MessagePack binary frames for the 1338 port).
/// Builders return `Arc<CachedActivity>` so broadcast fan-out shares the
/// single build instead of cloning per client (`mem-zero-copy`).
///
/// Both encodings are refcounted (`Utf8Bytes`/`Bytes`): cloning a payload
/// bumps counters, and builders adopt their serialization buffers with no
/// copy (`String`/`Vec<u8>` move straight in).
///
/// `json` deliberately stays `tungstenite::Utf8Bytes` instead of plain
/// `Bytes`: in tungstenite 0.30 `Message::Text` *is* `Utf8Bytes`, so the
/// broadcast fan-out clones with zero validation. Storing `Bytes` would
/// force a UTF-8 re-scan on every send — O(payload) per consumer instead
/// of O(1). The tungstenite dependency is the price of that guarantee.
#[derive(Clone, Debug)]
pub struct CachedActivity {
  pub json: Utf8Bytes,
  pub msgpack: Bytes,
  /// Whether this payload clears presence (`activity: null`). Recorded at
  /// construction so consumers never re-parse the JSON to find out.
  pub is_clear: bool,
  /// The serialized activity alone (flood-guard fingerprint source).
  /// Recorded at construction so change detection compares bytes instead
  /// of re-serializing and re-parsing; empty for clears.
  pub activity_json: Bytes,
  /// Detection provenance for snapshots and journal correlation
  /// (in-memory only — never serialized onto the wire). `None` for
  /// clears and unclassified payloads.
  pub provenance: Option<ActivityProvenance>,
  /// The pid this card belongs to, recorded at construction.
  ///
  /// The replay cache needs it for ghost reaping, and the snapshot needs
  /// it for its listing; both used to parse it back out of the serialized
  /// JSON to learn something the builder already knew.
  pub pid: u64,
}

/// Where a published card came from: detection matcher or SDK client,
/// plus process age at first publish when known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityProvenance {
  /// Matcher source (`automaton`, `steam-app-id`, …) or `"sdk"` for
  /// client-published activities.
  ///
  /// A `Cow` because both producers hand over a `'static` identifier — a
  /// matcher's wire string, or the literal `"sdk"` — so a publish never
  /// allocates to say where the card came from.
  pub source: Cow<'static, str>,
  /// Process age in ms at first publish (`None` when the start time
  /// was unreadable, always for SDK activities).
  pub latency_ms: Option<u64>,
}

/// Build the empty (clear) payload in both protocols. `None` when either
/// encoding fails: fixed shapes make that practically unreachable, and
/// callers log and drop so a future field addition surfaces loudly
/// instead of shipping an empty frame.
#[must_use]
pub fn empty_cached(pid: u64, socket_id: SocketId) -> Option<Arc<CachedActivity>> {
  let payload = ActivityPayload {
    activity: None,
    pid: Some(pid),
    socket_id: Some(socket_id.to_string()),
  };
  Some(Arc::new(CachedActivity {
    json: serde_json::to_string(&payload).ok().map(Utf8Bytes::from)?,
    msgpack: rmp_serde::to_vec_named(&payload).ok().map(Bytes::from)?,
    is_clear: true,
    activity_json: Bytes::new(),
    provenance: None,
    pid,
  }))
}

/// Serialize the command's activity alone: the flood-guard fingerprint.
/// Runs `fix()` and stamps the outer `application_id` first, so the bytes
/// match what [`cached_activity`] stores — two commands differing only in
/// the outer id must not share a fingerprint.
///
/// Returns `None` when there is no activity (a clear).
///
/// The bytes come back as [`Bytes`] so every later holder — the flood
/// guard, the cached payload — keeps a strong reference to one buffer
/// instead of re-copying the serialization.
#[must_use]
pub fn activity_fingerprint(cmd: &mut ActivityCmd) -> Option<Bytes> {
  cmd.fix();
  let application_id = cmd.application_id.clone();
  let activity = cmd.args.as_mut()?.activity.as_mut()?;
  activity.application_id = application_id;
  serde_json::to_vec(activity).ok().map(Bytes::from)
}

/// Turn a `SET_ACTIVITY` command into the bridge payload in both protocols.
///
/// `fingerprint` should be the [`activity_fingerprint`] output for `cmd`
/// (computed once, up front, so flood-dropped publishes never pay for the
/// envelope build); it is stored on the payload for byte-compare change
/// detection. A missing fingerprint degrades to always-changed, never to
/// a dropped broadcast.
///
/// Takes the fingerprint as [`Bytes`] and stores it as it is: no copy of
/// the serialization is made on the way in.
///
/// Consumes `cmd`: the activity moves into the payload instead of being
/// cloned for it. Callers that still need the command afterwards (logs,
/// the SDK handoff, clear resume) read those facts before handing it over —
/// this function is the last thing that runs before the fan-out.
///
/// Returns `None` when the command cannot be converted into a valid payload
/// (e.g. it is missing its arguments entirely).
#[must_use]
pub fn cached_activity(
  mut cmd: ActivityCmd,
  fingerprint: Option<Bytes>,
) -> Option<Arc<CachedActivity>> {
  cmd.fix();

  // Owned command: take the activity out instead of cloning it. The
  // application id moves too — nothing else reads it afterwards.
  let mut args = cmd.args.take()?;

  if args.activity.is_none() {
    let pid = args.pid.unwrap_or_default();
    return empty_cached(pid, SocketId::from(pid.to_string()));
  }

  let mut activity = args.activity.take()?;
  activity.application_id = cmd.application_id;

  let payload = ActivityPayload {
    activity: Some(activity),
    pid: args.pid,
    socket_id: Some(args.pid.unwrap_or(0).to_string()),
  };

  Some(Arc::new(CachedActivity {
    json: serde_json::to_string(&payload).ok()?.into(),
    msgpack: rmp_serde::to_vec_named(&payload).ok().map(Bytes::from)?,
    // Reached only when `args.activity` is `Some`: the serialized body
    // carries a real activity object, never null.
    is_clear: false,
    activity_json: fingerprint.unwrap_or_default(),
    // Provenance is stamped by the bridge caller (generic vs SDK paths
    // know their source); the protocol layer leaves it empty.
    provenance: None,
    pid: args.pid.unwrap_or_default(),
  }))
}

/// Build the acknowledgement for a `SUBSCRIBE`/`UNSUBSCRIBE` command:
/// echoes `cmd`/`nonce`, reports the subscribed event name in `data.evt`
/// (the official shape; arRPC blind-ACKs the same way).
///
/// # Blind-ACK scope
///
/// The ACK confirms receipt only — there is no backend behind most event
/// families, so a subscription that is ACKed here will simply never fire.
/// Emergency error frame for the (practically unreachable) case where
/// the structured builders below fail to serialize: `cmd` is escaped
/// through serde instead of interpolated raw, so the fallback is always
/// valid JSON. `evt` renders as given or JSON null when `None`, mirroring
/// each caller's primary shape. The primaries only fail on shapes that
/// cannot occur (fixed String/int fields), which is why this stays a
/// fallback instead of a `Result`.
fn error_fallback_json(cmd: &str, evt: Option<&str>) -> String {
  let safe_cmd = serde_json::to_string(cmd).unwrap_or_else(|_| r#""?""#.to_string());
  match evt {
    Some(evt) => {
      let safe_evt = serde_json::to_string(evt).unwrap_or_else(|_| r#""?""#.to_string());
      format!(r#"{{"cmd":{safe_cmd},"evt":{safe_evt}}}"#)
    }
    None => format!(r#"{{"cmd":{safe_cmd},"evt":null}}"#),
  }
}

/// Borrowed reply envelope: `{cmd, data, evt, nonce}`, the shape every
/// reply on this path shares.
///
/// Serializing by reference avoids the intermediate `Value` tree a
/// `json!` macro builds for each reply — one tree and one final String
/// instead of two trees and a String. `data` is generic so each caller
/// passes either a borrowed value, a fixed-shape struct, or `&Value::NULL`
/// for a literal null.
#[derive(Serialize)]
struct Reply<'a, D> {
  cmd: &'a str,
  data: D,
  evt: Option<&'a str>,
  nonce: &'a Value,
}

/// JSON null, for replies whose `data` is literally null.
static NULL_DATA: Value = Value::Null;

/// `data` for the subscribe acknowledgement: the event the client asked
/// for, echoed, or null when it named none.
#[derive(Serialize)]
struct SubscribeData<'a> {
  evt: Option<&'a str>,
}

/// `data` for an error reply: the code and human message.
#[derive(Serialize)]
struct ErrorData<'a> {
  code: u16,
  message: &'a str,
}

/// Events this server can actually dispatch: `READY` (on connect),
/// `ERROR` (command failures) and `CURRENT_USER_UPDATE` (bridge identity
/// changes via `SET_USER`/`RESET_USER`). Everything else in the official
/// table is accepted and then silent, because it needs the real Discord
/// client: voice (`VOICE_*`, `SPEAKING_*`), guilds/channels
/// (`GUILD_*`, `CHANNEL_CREATE`), messages/notifications
/// (`MESSAGE_*`, `NOTIFICATION_CREATE`), activity invites
/// (`ACTIVITY_JOIN`, `ACTIVITY_SPECTATE`, `ACTIVITY_JOIN_REQUEST`,
/// `ACTIVITY_INVITE`), relationships (`RELATIONSHIP_UPDATE`) and store
/// (`ENTITLEMENT_CREATE`, `ENTITLEMENT_DELETE`).
#[must_use]
pub fn subscribe_ack(cmd: &ActivityCmd) -> String {
  serde_json::to_string(&Reply {
    cmd: &cmd.cmd,
    data: SubscribeData {
      evt: cmd.evt.as_deref(),
    },
    evt: None,
    nonce: &cmd.nonce,
  })
  .unwrap_or_else(|_| error_fallback_json(&cmd.cmd, Some("ERROR")))
}

/// Build the reply for a `GET_USER` command: the current identity, or
/// `null` when the requested id names somebody else (the official
/// response is "an RPC user object or null").
#[must_use]
pub fn user_response(cmd: &ActivityCmd, user: Option<&RpcUser>) -> String {
  // `data` is the user object itself or null; borrowing the server's own
  // value avoids rebuilding the envelope around it.
  let data = user.and_then(|user| serde_json::to_value(user).ok());
  serde_json::to_string(&Reply {
    cmd: &cmd.cmd,
    data: data.as_ref().unwrap_or(&NULL_DATA),
    evt: None,
    nonce: &cmd.nonce,
  })
  .unwrap_or_else(|_| error_fallback_json(&cmd.cmd, Some("ERROR")))
}

/// Build the `CURRENT_USER_UPDATE` dispatch emitted when the local
/// identity changes (`SET_USER`/`RESET_USER`). The inner payload is the
/// user object itself, per the official event shape.
#[must_use]
pub fn current_user_update(user: &RpcUser) -> String {
  serde_json::to_string(&Reply {
    cmd: "DISPATCH",
    data: user,
    evt: Some("CURRENT_USER_UPDATE"),
    nonce: &NULL_DATA,
  })
  .unwrap_or_else(|_| String::from("{}"))
}

/// Official error for a known command that has no backend here (OAuth,
/// voice, guilds, overlay, store...). Returns `(code, message)` so the IPC
/// and WebSocket dispatches share one table instead of drifting apart;
/// `None` means "not a known-unbacked command" (handled elsewhere, or
/// genuinely unknown).
#[must_use]
pub fn unsupported_command(cmd: &str) -> Option<(u16, &'static str)> {
  const NEEDS_CLIENT: &str = "requires the real Discord client";
  match cmd {
    // OAuth needs the in-client modal plus token exchange with the app's
    // secret: 5000 is the official OAuth2 error bucket.
    "AUTHORIZE" | "AUTHENTICATE" => Some((5000, "Authorization requires the real Discord client")),
    // Activity invites target live sessions on the real client.
    "SEND_ACTIVITY_JOIN_INVITE"
    | "CLOSE_ACTIVITY_REQUEST"
    | "ACCEPT_ACTIVITY_INVITE"
    | "ACTIVITY_INVITE_USER" => Some((
      5006,
      "No eligible activity: invites require the real Discord client",
    )),
    // Voice, guilds, channels, overlay, store, capture, certs: real-client
    // state only. Grouped under the generic code with an honest message.
    "GET_GUILD"
    | "GET_GUILDS"
    | "GET_CHANNEL"
    | "GET_CHANNELS"
    | "CREATE_CHANNEL_INVITE"
    | "GET_RELATIONSHIPS"
    | "SET_USER_VOICE_SETTINGS"
    | "SET_USER_VOICE_SETTINGS_2"
    | "PUSH_TO_TALK"
    | "SELECT_VOICE_CHANNEL"
    | "GET_SELECTED_VOICE_CHANNEL"
    | "SELECT_TEXT_CHANNEL"
    | "GET_VOICE_SETTINGS"
    | "SET_VOICE_SETTINGS"
    | "SET_VOICE_SETTINGS_2"
    | "SET_CERTIFIED_DEVICES"
    | "CAPTURE_SHORTCUT"
    | "GET_IMAGE"
    | "OVERLAY"
    | "SET_OVERLAY_LOCKED"
    | "OPEN_OVERLAY_ACTIVITY_INVITE"
    | "OPEN_OVERLAY_GUILD_INVITE"
    | "OPEN_OVERLAY_VOICE_SETTINGS"
    | "GET_SKUS"
    | "GET_ENTITLEMENTS"
    | "START_PURCHASE"
    | "VALIDATE_APPLICATION" => Some((1000, NEEDS_CLIENT)),
    _ => None,
  }
}

/// Build an `evt: "ERROR"` reply for a command the server refuses
/// (unknown command, invalid invite code, unsupported callback, ...),
/// mirroring arRPC's `{cmd, data: {code, message}, evt: "ERROR", nonce}`.
#[must_use]
pub fn rpc_error(cmd: &str, nonce: &Value, code: u16, message: &str) -> String {
  serde_json::to_string(&Reply {
    cmd,
    data: ErrorData { code, message },
    evt: Some("ERROR"),
    nonce,
  })
  .unwrap_or_else(|_| error_fallback_json(cmd, Some("ERROR")))
}

/// Build a neutral acknowledgement for known secondary commands
/// (`INVITE_BROWSER`, `DEEP_LINK`, ...) that are forwarded to bridge
/// clients: the outcome lives downstream, so the reply only confirms
/// receipt (arRPC answers these from the bridge round-trip; without
/// bridge clients there is nothing more to report).
#[must_use]
pub fn generic_ack(cmd: &ActivityCmd) -> String {
  serde_json::to_string(&Reply {
    cmd: &cmd.cmd,
    data: &NULL_DATA,
    evt: None,
    nonce: &cmd.nonce,
  })
  .unwrap_or_else(|_| error_fallback_json(&cmd.cmd, None))
}

/// The `data` object of a `SET_ACTIVITY` echo: the activity's own fields
/// followed by the command's `application_id`.
///
/// The id is emitted from the command rather than from the activity, so
/// [`set_activity_response`] clears the activity's copy first — otherwise
/// `flatten` writes the activity's `application_id` and this field writes
/// the command's, and the reply contains the key twice.
///
/// No `skip_serializing_if` on the id: when the command carries none the
/// echo must say `"application_id": null`, which `None` gives for free,
/// not omit the key.
#[derive(Serialize)]
struct ActivityEcho<'a> {
  #[serde(flatten)]
  activity: &'a Activity,
  application_id: Option<&'a str>,
}

/// Build the official-shaped confirmation reply for a `SET_ACTIVITY` command.
///
/// The reply echoes `cmd`/`nonce` and carries `data` with the (fixed)
/// activity **exactly as the game sent it** — `name`, `type` (Playing /
/// Listening / Watching / Competing) and every other field are preserved,
/// matching the official echo semantics. The lock-step guarantee RPC
/// libraries rely on (e.g. pypresence must receive *some* reply or it
/// hangs) comes from always answering, never from rewriting the body, so
/// strict clients validating the echo see their own activity back.
/// `application_id` (known from the handshake, absent from what the game
/// sent) is attached as a routing enrichment; unknown keys tolerate it.
/// Returns `None` when the command has no arguments.
///
/// Takes `&mut` because the echo's `application_id` is the command's, not
/// the activity's: the activity's own copy is dropped first so the field
/// below is the only one emitted (see the echo envelope below).
#[must_use]
pub fn set_activity_response(cmd: &mut ActivityCmd) -> Option<String> {
  // The echo's `application_id` is the outer command's id, verbatim —
  // `null` when the command carries none — overriding whatever the
  // activity itself sent. Take the activity's own copy out so the DTO
  // field below is the only source: left in, `flatten` would write the
  // key a second time and the game would see two `application_id`s.
  // Dropping it also costs nothing: the string is discarded, not cloned.
  if let Some(activity) = cmd.args.as_mut().and_then(|args| args.activity.as_mut()) {
    activity.application_id = None;
  }

  let args = cmd.args.as_ref()?;
  let data = args.activity.as_ref().map(|activity| ActivityEcho {
    activity,
    application_id: cmd.application_id.as_deref(),
  });

  // Envelope and activity are serialized by reference, straight into the
  // reply. The previous build round-tripped the activity through a
  // `serde_json::Value` (a whole `Map`) just to overwrite one key, then
  // serialized that tree — on every publish. Keys inside `data` now
  // follow declaration order instead of the alphabet, which the echo
  // contract allows ("byte equivalent modulo key order").
  #[derive(Serialize)]
  struct Reply<'a> {
    cmd: &'a str,
    data: Option<ActivityEcho<'a>>,
    evt: Option<&'a str>,
    nonce: &'a Value,
  }

  serde_json::to_string(&Reply {
    cmd: &cmd.cmd,
    data,
    evt: None,
    nonce: &cmd.nonce,
  })
  .ok()
}

/// Flood guard for `SET_ACTIVITY`: collapses byte-identical republishes
/// from the same `(application_id, pid)` arriving inside a short window.
///
/// The official client throttles presence updates; without a guard here a
/// spinning or buggy SDK resending the same bytes at Hz rates would fan
/// out to every bridge consumer at full rate. Only *exact duplicates* are
/// dropped — any changed byte passes — and clears always pass and re-arm
/// the slot, so a wrong drop costs at most one stale frame for `window`,
/// self-healed by the next distinct publish (or the periodic bridge
/// refresh). The game already received its echo upstream, so lock-step
/// clients that require a reply never hang on a dropped duplicate.
#[derive(Clone, Debug)]
pub struct RecentActivities {
  window: std::time::Duration,
  cap: usize,
  /// Keyed by `(app_id, pid)`, hashed with `FxHash` like every other
  /// table in the workspace: the keys come from our own parsed command,
  /// not from an attacker-chosen login, so SipHash's DoS resistance buys
  /// nothing and costs hashing on the flood path.
  ///
  /// The id is an [`AppId`] because the caller holds one (`ActivityCmd`
  /// carries it refcounted): keying by it makes recording a refcount bump
  /// instead of the `String` allocation a borrowed `&str` key forced on
  /// every publish. `None` is its own slot — a command with no id is not
  /// the same as one with an empty id, and neither is worth allocating a
  /// sentinel for.
  entries: rustc_hash::FxHashMap<(Option<AppId>, u64), (Bytes, std::time::Instant)>,
}

impl RecentActivities {
  /// Canonical guard: 5s window (conservative — healthy SDK heartbeats
  /// re-send every 15s+, so only floods collapse).
  pub const DEFAULT_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
  /// Canonical table bound (far above the handful of co-running games;
  /// stops untrusted input from growing the map forever).
  pub const DEFAULT_CAP: usize = 128;

  /// Track duplicates inside `window`, keeping at most `cap` slots.
  #[must_use]
  pub fn new(window: std::time::Duration, cap: usize) -> Self {
    Self {
      window,
      cap,
      entries: rustc_hash::FxHashMap::default(),
    }
  }

  /// Number of slots currently remembered (for tests and diagnostics).
  #[must_use]
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether `len` reports no remembered slot.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// The payload recorded for `(app_id, pid)`, if any (tests and
  /// diagnostics). Shares its buffer with the caller that tracked it.
  #[must_use]
  pub fn recorded(&self, app_id: Option<&AppId>, pid: u64) -> Option<&Bytes> {
    self.entries.get(&(app_id.cloned(), pid)).map(|(fp, _)| fp)
  }

  /// The app id recorded for `(app_id, pid)`, if any.
  ///
  /// Shares its refcount with the caller: this is how a test proves the
  /// key is borrowed rather than rebuilt. The probe clones the `Arc`, which
  /// is a refcount bump — no allocation, which is the whole point.
  #[must_use]
  pub fn recorded_key(&self, app_id: Option<&AppId>, pid: u64) -> Option<&AppId> {
    self
      .entries
      .get_key_value(&(app_id.cloned(), pid))
      .and_then(|(key, _)| key.0.as_ref())
  }

  /// `true` when this exact payload was already seen from `(app_id, pid)`
  /// inside the window — the caller should drop it without broadcasting.
  /// Records the payload otherwise. A `None` payload (a clear) always
  /// returns `false` and forgets the slot, so the next publish — even
  /// byte-identical to a pre-clear one — is forwarded.
  ///
  /// Borrows the fingerprint: the flood path passes the one it already
  /// holds, and recording keeps a strong reference to the same buffer
  /// instead of a second copy.
  pub fn should_drop(
    &mut self,
    app_id: Option<&AppId>,
    pid: u64,
    payload: Option<&Bytes>,
    now: std::time::Instant,
  ) -> bool {
    // One key per call, built once: the probe borrows it, the insert moves
    // it. Cloning an `AppId` is a refcount bump, so tracking a publish
    // allocates nothing for the key — the `&str` form this replaces paid a
    // `String` on every call.
    let key = (app_id.cloned(), pid);
    let Some(bytes) = payload else {
      self.entries.remove(&key);
      return false;
    };
    if let Some((last, at)) = self.entries.get(&key)
      && *last == *bytes
      && now.duration_since(*at) < self.window
    {
      return true;
    }
    // Evict-then-insert, bounded: purge expired slots first (the actual
    // garbage), then fall back to one arbitrary eviction so hostile input
    // cannot grow the map without bound.
    if self.entries.len() >= self.cap && !self.entries.contains_key(&key) {
      self
        .entries
        .retain(|_, (_, at)| now.duration_since(*at) < self.window);
      if self.entries.len() >= self.cap
        && let Some(victim) = self.entries.keys().next().cloned()
      {
        self.entries.remove(&victim);
      }
    }
    self.entries.insert(key, (bytes.clone(), now));
    false
  }
}

impl Default for RecentActivities {
  fn default() -> Self {
    Self::new(Self::DEFAULT_WINDOW, Self::DEFAULT_CAP)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Hostile command names (quotes, backslashes, control chars) escape
  /// through serde instead of breaking the emergency frame, in both
  /// shapes (error event and null event).
  #[test]
  fn error_fallback_escapes_hostile_commands() {
    let hostile = "x\"\\\n\x00y";
    let frame = error_fallback_json(hostile, Some("ERROR"));
    let parsed: serde_json::Value = serde_json::from_str(&frame).expect("fallback is valid JSON");
    assert_eq!(parsed["cmd"], hostile);
    assert_eq!(parsed["evt"], "ERROR");
    let null_frame = error_fallback_json(hostile, None);
    let null_parsed: serde_json::Value =
      serde_json::from_str(&null_frame).expect("null fallback is valid JSON");
    assert_eq!(null_parsed["cmd"], hostile);
    assert!(null_parsed["evt"].is_null());
  }

  /// The reply fallback is itself valid JSON: a failure to serialize the
  /// real reply must never emit a broken frame to a lock-step client.
  #[test]
  fn error_fallback_stays_valid_json() {
    for evt in [None, Some("ERROR")] {
      let fallback = error_fallback_json("SET_ACTIVITY", evt);
      let parsed: Value = serde_json::from_str(&fallback).expect("fallback must be valid JSON");
      assert_eq!(parsed["cmd"], "SET_ACTIVITY");
      assert_eq!(parsed["evt"], evt.map_or(Value::Null, |e| e.into()));
    }
  }

  /// N20: the flood guard hashes with the workspace's `FxHashMap`, not
  /// `std`'s `SipHash`.
  ///
  /// The guard runs once per publish over `(String, u64)` keys — small,
  /// and derived from our own parsed command rather than from an
  /// attacker-chosen login, so SipHash's DoS resistance buys nothing here
  /// and costs hashing on the hot path. Every other table in the
  /// workspace already uses `FxHash`.
  ///
  /// Structural, because the win is hashing cost: no behavioral assertion
  /// can tell the two hashers apart, they just produce different bucket
  /// orders. The helper's parameter type is the assertion — it stops
  /// compiling if the field ever reverts.
  #[test]
  fn flood_guard_uses_the_workspace_hasher() {
    // Compiles only for a `FxHashMap`.
    fn assert_fx<K: Eq + std::hash::Hash, V>(_: &rustc_hash::FxHashMap<K, V>) {}
    let guard = RecentActivities::default();
    assert_fx(&guard.entries);
  }
}
