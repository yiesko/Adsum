use adsum_oauth::store::{PersistedSession, clear, load, save};

#[test]
fn save_and_load_roundtrip() {
  let session = PersistedSession {
    client_id: "123456789012345678".to_owned(),
    refresh_token: Some("test-refresh-token".to_owned()),
    scopes: vec!["rpc".to_owned(), "identify".to_owned()],
    expires: 9999999999,
  };
  save(&session).unwrap();
  let loaded = load("123456789012345678").unwrap();
  assert_eq!(loaded.refresh_token, session.refresh_token);
  assert_eq!(loaded.scopes, session.scopes);
  assert!(!loaded.is_expired());
  clear("123456789012345678");
}

#[test]
fn load_missing_returns_none() {
  clear("999999999999999999");
  assert!(load("999999999999999999").is_none());
}

#[test]
fn clear_removes_file() {
  let session = PersistedSession {
    client_id: "111111111111111111".to_owned(),
    refresh_token: None,
    scopes: vec![],
    expires: 1,
  };
  save(&session).unwrap();
  assert!(load("111111111111111111").is_some());
  clear("111111111111111111");
  assert!(load("111111111111111111").is_none());
}

#[test]
fn expired_session_detected() {
  let session = PersistedSession {
    client_id: "222222222222222222".to_owned(),
    refresh_token: None,
    scopes: vec![],
    expires: 1, // unix epoch + 1s → expired
  };
  assert!(session.is_expired());
  clear("222222222222222222");
}

#[test]
fn invalid_client_id_rejected() {
  let session = PersistedSession {
    client_id: "abc".to_owned(), // not digits
    refresh_token: None,
    scopes: vec![],
    expires: 9999999999,
  };
  assert!(save(&session).is_err());
}
