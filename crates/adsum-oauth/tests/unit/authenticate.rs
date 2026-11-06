use super::*;
use adsum_types::cmd::ActivityCmd;

#[test]
fn fetch_discord_json_rejects_unsafe_paths() {
  for bad in [
    "",
    "users/@me/guilds",
    "//evil.com/x",
    "/users/../secret",
    "/a b/c",
    "/a\\b",
  ] {
    let err = crate::authenticate::fetch_discord_json("token", bad).unwrap_err();
    assert_eq!(err, crate::OAuthError::InvalidArguments, "path {bad:?}");
  }
}

#[test]
fn fetch_discord_json_accepts_api_paths() {
  // We cannot hit the network in unit tests; only the validation gate is
  // exercised here. A valid path passes validation and fails later at the
  // HTTP layer, never with InvalidArguments.
  for good in ["/users/@me/guilds", "/users/@me", "/oauth2/@me"] {
    let err = crate::authenticate::fetch_discord_json("token", good).unwrap_err();
    assert_ne!(err, crate::OAuthError::InvalidArguments, "path {good:?}");
  }
}

#[test]
fn rfc3339_expiry_is_parsed_with_timezone_offset() {
  let parsed = crate::helpers::parse_rfc3339("2017-06-29T19:09:52.361000+00:00").unwrap();
  assert_eq!(
    parsed
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs(),
    1_498_763_392
  );
  let offset = crate::helpers::parse_rfc3339("2017-06-29T21:09:52+02:00").unwrap();
  assert_eq!(
    offset
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs(),
    1_498_763_392
  );
}

#[test]
fn rfc3339_rejects_invalid_calendar_dates_and_untrusted_overflow() {
  assert!(crate::helpers::parse_rfc3339("2025-02-29T10:00:00Z").is_none());
  assert!(crate::helpers::parse_rfc3339("2024-02-29T10:00:00Z").is_some());
  assert!(crate::helpers::parse_rfc3339("2024-04-31T10:00:00Z").is_none());
  assert!(crate::helpers::parse_rfc3339("999999999999999999999-01-01T00:00:00Z").is_none());
  assert!(crate::helpers::parse_rfc3339("2024-01-01T00:00:00.badZ").is_none());
}

#[test]
fn rpc_auth_arguments_survive_flattened_command_deserialization() {
  let command: ActivityCmd = serde_json::from_value(serde_json::json!({
      "cmd": "AUTHORIZE",
      "args": {
          "client_id": "123456789012345678",
          "scopes": ["rpc", "identify"],
          "code_challenge": "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
          "code_challenge_method": "S256"
      },
      "nonce": "test"
  }))
  .unwrap();
  let extra = &command.args.unwrap().extra;
  assert_eq!(
    extra.get("client_id").and_then(serde_json::Value::as_str),
    Some("123456789012345678")
  );
  assert_eq!(
    extra
      .get("code_challenge_method")
      .and_then(serde_json::Value::as_str),
    Some("S256")
  );
}

#[test]
fn wrong_application_is_rejected_before_starting_browser() {
  let command: ActivityCmd = serde_json::from_value(serde_json::json!({
      "cmd": "AUTHORIZE",
      "args": {"client_id": "123456789012345678", "scopes": ["rpc"]},
      "nonce": "test"
  }))
  .unwrap();
  assert_eq!(
    crate::handle_rpc_command(&command, "999999999999999999").unwrap_err(),
    crate::OAuthError::InvalidClientId
  );
}

#[test]
fn authenticate_response_requires_rpc_scope_and_matching_application() {
  let valid = serde_json::json!({
      "application": { "id": "123456789012345678", "name": "OAuth test" },
      "scopes": ["rpc", "identify"],
      "expires": "2099-01-01T00:00:00+00:00",
      "user": {
          "id": "987654321098765432",
          "username": "oauth-test-user",
          "discriminator": "0"
      }
  });
  let session = session_from_auth_response(&valid, "123456789012345678", "test-token").unwrap();
  assert_eq!(session.user.as_ref().unwrap().id, "987654321098765432");
  assert!(session.scopes.iter().any(|scope| scope == "rpc"));
  assert!(!session.is_expired());

  let no_rpc = serde_json::json!({
      "application": { "id": "123456789012345678" },
      "scopes": ["identify"],
      "expires": "2099-01-01T00:00:00+00:00",
      "user": null
  });
  assert_eq!(
    session_from_auth_response(&no_rpc, "123456789012345678", "test-token").unwrap_err(),
    crate::OAuthError::MissingRpcScope
  );
  assert_eq!(
    session_from_auth_response(&valid, "999999999999999999", "test-token").unwrap_err(),
    crate::OAuthError::ApplicationMismatch
  );
}

#[test]
fn token_validation_rejects_header_injection() {
  assert_eq!(
    crate::authenticate("token\r\nX-Evil: true", "123456789012345678").unwrap_err(),
    crate::OAuthError::InvalidAccessToken
  );
}
