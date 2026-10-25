use super::*;
use std::collections::HashMap;

/// Newtypes stay pointer-sized: no inline buffer beside the `Arc`.
#[test]
fn ids_share_arc_allocation() {
  assert_eq!(
    std::mem::size_of::<AppId>(),
    std::mem::size_of::<Arc<str>>()
  );
  assert_eq!(
    std::mem::size_of::<SocketId>(),
    std::mem::size_of::<Arc<str>>()
  );
}

/// Clones share the allocation (pointer-equal), never copy the bytes.
#[test]
fn clone_shares_instead_of_copying() {
  let id = AppId::from("123456789012345678");
  let cloned = id.clone();
  assert!(std::ptr::eq(id.as_ref().as_ptr(), cloned.as_ref().as_ptr()));
  let sock = SocketId::from("sock");
  let sock_cloned = sock.clone();
  assert!(std::ptr::eq(
    sock.as_ref().as_ptr(),
    sock_cloned.as_ref().as_ptr()
  ));
}

/// A pid renders into the same decimal the old `pid.to_string()` path
/// produced, or every cached socket key would silently change identity.
#[test]
fn socket_id_from_u64_matches_the_decimal_string() {
  for pid in [0u64, 1, 7, 42, 999, 65_535, 4_294_967_295, u64::MAX] {
    let from_pid = SocketId::from(pid);
    assert_eq!(
      from_pid.as_ref(),
      pid.to_string(),
      "pid {pid} must render identically"
    );
    assert_eq!(from_pid, SocketId::from(pid.to_string().as_str()));
  }
}

/// `Borrow<str>` lets maps answer `&str` lookups with no owned key.
#[test]
fn str_lookup_needs_no_owned_key() {
  let mut map: HashMap<AppId, u64> = HashMap::new();
  map.insert(AppId::from("game-1"), 42);
  assert_eq!(map.get("game-1"), Some(&42));
  let mut sockets: HashMap<SocketId, u64> = HashMap::new();
  sockets.insert(SocketId::from("sock-1"), 7);
  assert_eq!(sockets.get("sock-1"), Some(&7));
}

/// Every `From` direction preserves the exact id string.
#[test]
fn conversions_roundtrip() {
  let app = AppId::from("abc");
  assert_eq!(app.to_string(), "abc");
  assert_eq!(String::from(app), "abc");
  let sock = SocketId::from(&AppId::from("abc"));
  assert_eq!(sock.as_ref(), "abc");
  let moved = SocketId::from(AppId::from("abc"));
  assert_eq!(moved.as_ref(), "abc");
}

/// Boxed-slice adoption shares on later clones (pointer-equal).
#[test]
fn boxed_str_converts() {
  // One copy into the refcounted layout (Arc stores counters inline, so
  // in-place adoption is impossible); every later clone is free.
  let id = AppId::from(Box::<str>::from("boxed-id"));
  assert_eq!(id.as_ref(), "boxed-id");
  let cloned = id.clone();
  assert!(std::ptr::eq(id.as_ref().as_ptr(), cloned.as_ref().as_ptr()));
  let sock = SocketId::from(Box::<str>::from("sock-id"));
  assert_eq!(sock.as_ref(), "sock-id");
}

/// Serde wire format is a plain JSON string, both directions.
#[test]
fn wire_format_is_plain_string() {
  let json = serde_json::to_string(&AppId::from("123")).unwrap();
  assert_eq!(json, r#""123""#);
  let back: AppId = serde_json::from_str(&json).unwrap();
  assert_eq!(back, AppId::from("123"));
}

/// `Deref<Target = str>` lets the ids stand in for `&str` wherever the
/// pipeline already expects one.
///
/// This is the missing piece that makes D7 and C3 mechanical instead of
/// invasive: `Option<AppId>::as_deref()` is the call shape used at ~8
/// production sites today, and without the impl every one of them has
/// to spell out a conversion by hand.
#[test]
fn ids_deref_to_str_and_as_deref_works() {
  let id = AppId::from("12345");
  // Coercion: `&AppId` where `&str` is expected.
  let s: &str = &id;
  assert_eq!(s, "12345");
  // `str` methods resolve through the deref, no import needed.
  assert_eq!(id.len(), 5);
  assert!(id.starts_with("123"));

  // The pipeline's shape.
  let some: Option<AppId> = Some(AppId::from("12345"));
  assert_eq!(some.as_deref(), Some("12345"));
  let none: Option<AppId> = None;
  assert_eq!(none.as_deref(), None);

  // `SocketId` mirrors `AppId` (see `Borrow<str>` above).
  let sock = SocketId::from("sock-1");
  let s2: &str = &sock;
  assert_eq!(s2, "sock-1");
  let some_sock: Option<SocketId> = Some(SocketId::from("sock-1"));
  assert_eq!(some_sock.as_deref(), Some("sock-1"));
}
