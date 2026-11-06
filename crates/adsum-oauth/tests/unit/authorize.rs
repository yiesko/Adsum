use super::*;

#[test]
fn redirect_requires_a_registered_unprivileged_loopback_callback() {
  assert!(parse_loopback_redirect("http://127.0.0.1:48152/callback").is_ok());
  assert!(parse_loopback_redirect("https://127.0.0.1:48152/callback").is_err());
  assert!(parse_loopback_redirect("http://example.com:48152/callback").is_err());
  assert!(parse_loopback_redirect("http://127.0.0.1:80/callback").is_err());
  assert!(parse_loopback_redirect("http://127.0.0.1:48152/callback?x=1").is_err());
  assert!(parse_loopback_redirect("http://127.0.0.1:48152/callback/extra").is_err());
}

#[test]
fn duplicate_oauth_query_parameters_are_rejected() {
  assert!(parse_query("state=a&state=b&code=x").is_none());
  assert!(parse_query("code=a%2Bb&state=abc%2F123").is_some());
}
