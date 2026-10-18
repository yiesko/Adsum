use adsum_protocol::query::query_param;

/// `key=value` pairs read straight off the URI: bare flags, missing
/// queries and empty inputs yield nothing.
#[test]
fn reads_pairs_and_ignores_bare_flags() {
  let uri = "/?v=1&encoding=json&client_id=abc";
  assert_eq!(query_param(uri, "v"), Some("1"));
  assert_eq!(query_param(uri, "encoding"), Some("json"));
  assert_eq!(query_param(uri, "client_id"), Some("abc"));
  assert_eq!(query_param("/", "v"), None);
  assert_eq!(query_param("/?flag", "flag"), None);
  assert_eq!(query_param("", "v"), None);
  // A key must match the whole segment before `=`, never a substring.
  assert_eq!(query_param("/?encoding=json", "encoding"), Some("json"));
  assert_eq!(query_param("/?encoding=json", "enc"), None);
  assert_eq!(query_param("/?xclient_id=no", "client_id"), None);
}

/// A repeated key keeps answering the way the old `HashMap` did: the last
/// occurrence wins, so `insert` behaviour is preserved without the map.
#[test]
fn repeated_keys_keep_the_last_occurrence() {
  assert_eq!(
    query_param("/?client_id=one&client_id=two", "client_id"),
    Some("two")
  );
  assert_eq!(query_param("/?a=1&b=2&a=3", "a"), Some("3"));
  assert_eq!(query_param("/?a=1&b=2&a=3", "b"), Some("2"));
  assert_eq!(
    query_param("/?client_id=one&client_id=two", "missing"),
    None
  );
  // A bare repeat (no `=`) is ignored, so the valued one still answers.
  assert_eq!(
    query_param("/?client_id&client_id=abc", "client_id"),
    Some("abc")
  );
  assert_eq!(
    query_param("/?client_id=abc&client_id", "client_id"),
    Some("abc")
  );
}

/// The value is borrowed from the URI, so nothing is decoded or copied.
#[test]
fn values_come_back_as_uri_slices() {
  let uri = String::from("/?client_id=abc%20def&flag");
  let got = query_param(&uri, "client_id").expect("client_id");
  // Same allocation as the URI itself: a slice, not a `String`.
  assert!(std::ptr::eq(got.as_ptr(), uri[12..].as_ptr()));
}
