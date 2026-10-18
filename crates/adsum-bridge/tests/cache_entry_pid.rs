//! Unit tests for replay-cache pid resolution (`cache_entry_pid`).
//!
//! Pure function, no runtime. Precedence: the JSON body pid is
//! authoritative; the numeric socket id is only a fallback when the body
//! pid is unusable; pid 0 and garbage resolve to `None` (never proven
//! dead, never reaped).

use adsum_bridge::cache_entry_pid;
use adsum_protocol::commands::CachedActivity;
use adsum_types::SocketId;

/// Clear-payload fixture carrying a chosen pid in its JSON body.
fn cached_with(pid_json: &str) -> CachedActivity {
  // The field mirrors the body, exactly as the real builders do: they now
  // record the pid instead of leaving the cache to parse it back out.
  let pid: u64 = pid_json.parse().unwrap_or(0);
  CachedActivity {
    json: tungstenite::Utf8Bytes::from(format!(r#"{{"activity":null,"pid":{pid_json}}}"#)),
    msgpack: bytes::Bytes::new(),
    is_clear: true,
    activity_json: bytes::Bytes::new(),
    provenance: None,
    pid,
  }
}

/// A payload whose body is unusable *and* which records no pid: the
/// socket-id fallback path.
fn cached_broken() -> CachedActivity {
  CachedActivity {
    json: tungstenite::Utf8Bytes::from_static("not json"),
    msgpack: bytes::Bytes::new(),
    is_clear: true,
    activity_json: bytes::Bytes::new(),
    provenance: None,
    pid: 0,
  }
}

/// The JSON body pid is authoritative over a numeric socket id.
#[test]
fn body_pid_wins_over_numeric_socket_id() {
  // Generic scanner cards are cached under the numeric application id
  // with the real pid in the JSON body: the body is authoritative.
  let payload = cached_with("42");
  assert_eq!(cache_entry_pid(&SocketId::from("4242"), &payload), Some(42));
}

/// Unparseable bodies fall back to the numeric socket id.
#[test]
fn numeric_socket_id_used_when_body_unusable() {
  let broken = cached_broken();
  assert_eq!(
    cache_entry_pid(&SocketId::from("4242"), &broken),
    Some(4242)
  );
}

/// Non-numeric socket ids resolve through the body pid.
#[test]
fn falls_back_to_body() {
  let payload = cached_with("77");
  assert_eq!(
    cache_entry_pid(&SocketId::from("some-app-id"), &payload),
    Some(77)
  );
}

/// Zero and garbage resolve to `None` (never proven dead, never reaped).
#[test]
fn rejects_zero_and_garbage() {
  let payload = cached_with("0");
  assert_eq!(cache_entry_pid(&SocketId::from("0"), &payload), None);
  // A zero socket id no longer masks a usable body pid: the body is
  // authoritative, so this entry resolves to pid 9.
  let payload = cached_with("9");
  assert_eq!(cache_entry_pid(&SocketId::from("0"), &payload), Some(9));
  let broken = cached_broken();
  assert_eq!(cache_entry_pid(&SocketId::from("app"), &broken), None);
}

/// The recorded pid resolves without touching the JSON at all: a body
/// that would not parse still yields its pid, which is what removes the
/// re-parse from the ghost-reap path.
#[test]
fn recorded_pid_needs_no_json() {
  // A body that is not even valid JSON, but the builder knew the pid.
  let payload = CachedActivity {
    json: tungstenite::Utf8Bytes::from_static(""),
    msgpack: bytes::Bytes::new(),
    is_clear: true,
    activity_json: bytes::Bytes::new(),
    provenance: None,
    pid: 4242,
  };
  assert_eq!(
    cache_entry_pid(&SocketId::from("not-a-pid"), &payload),
    Some(4242)
  );
}
