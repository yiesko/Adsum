use super::*;

/// Direct matches win; labels only break ties.
#[test]
fn parse_body_prefers_direct_and_labels() {
  let (parsed, label) = parse_body(r#"[{"id":"1","name":"G","hook":false}]"#).unwrap();
  assert_eq!(label, "direct");
  assert_eq!(parsed.len(), 1);
}

/// Trimmed-name fallback still classifies when direct misses.
#[test]
fn parse_body_falls_back_through_trim() {
  // Missing `hook` (required without default): direct parse fails, the
  // trimmed projection defaults it.
  let (parsed, label) = parse_body(r#"[{"id":"1","name":"G"}]"#).unwrap();
  assert_eq!(label, "trimmed");
  assert_eq!(parsed[0].name, "G");
}

/// Garbage bodies classify to nothing (never panic).
#[test]
fn parse_body_rejects_garbage() {
  assert!(parse_body("not json{{").is_err());
}

/// The bundled snapshot loads and parses end to end.
#[test]
fn bundled_loads() {
  assert!(!load_bundled().expect("bundled snapshot parses").is_empty());
}
