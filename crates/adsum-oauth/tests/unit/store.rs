use super::*;

#[test]
fn empty_client_id_is_rejected() {
  let session = PersistedSession {
    client_id: String::new(),
    refresh_token: None,
    scopes: vec![],
    expires: 0,
  };
  assert!(save(&session).is_err());
}

#[test]
fn non_numeric_client_id_is_rejected() {
  let session = PersistedSession {
    client_id: "abc".to_owned(),
    refresh_token: None,
    scopes: vec![],
    expires: 0,
  };
  assert!(save(&session).is_err());
}
