use super::*;

/// Absent origin (native clients) passes; Discord's pages pass; an
/// explicit extra origin passes; anything else is refused.
#[test]
fn origin_policy() {
  let extra = vec!["https://my-client.example".to_string()];
  assert!(origin_allowed(None, &[]));
  assert!(origin_allowed(None, &extra));
  assert!(origin_allowed(Some("https://discord.com"), &[]));
  assert!(origin_allowed(Some("https://canary.discord.com"), &[]));
  assert!(origin_allowed(Some("https://ptb.discord.com"), &[]));
  assert!(origin_allowed(Some("https://my-client.example"), &extra));
  assert!(!origin_allowed(Some("https://evil.example"), &[]));
  assert!(!origin_allowed(Some("https://evil.example"), &extra));
  assert!(!origin_allowed(Some("http://discord.com"), &[]));
}

/// Configured entries canonicalize to browser serialization:
/// trailing slashes, casing and default ports stop mattering, while
/// non-default ports are preserved.
#[test]
fn extra_origins_normalize() {
  assert_eq!(
    normalize_origin("https://my-client.example/"),
    "https://my-client.example"
  );
  assert_eq!(
    normalize_origin("HTTPS://My-Client.Example"),
    "https://my-client.example"
  );
  assert_eq!(
    normalize_origin("https://my-client.example:443"),
    "https://my-client.example"
  );
  assert_eq!(
    normalize_origin("http://my-client.example:80/"),
    "http://my-client.example"
  );
  assert_eq!(
    normalize_origin("https://my-client.example:8443"),
    "https://my-client.example:8443"
  );
  assert_eq!(
    normalize_origin("  https://my-client.example  "),
    "https://my-client.example"
  );
}

/// A mixed-case configured entry with a trailing slash matches the
/// serialized origin the browser actually sends.
#[test]
fn normalized_extras_match_browser_origins() {
  let extra = vec![normalize_origin("HTTPS://My-Client.Example/")];
  assert!(origin_allowed(Some("https://my-client.example"), &extra));
  assert!(!origin_allowed(Some("https://evil.example"), &extra));
}
