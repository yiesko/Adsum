use super::*;

/// `trim_parse` must produce exactly what `trim_detectable_value` plus a
/// struct parse produces. The whole point of the struct path is to avoid
/// the DOMs, never to change the projection — so this is the contract.
fn assert_paths_agree(body: &str) {
  let via_value: Vec<DetectableActivity> =
    serde_json::from_value(trim_detectable_value(body).expect("value path")).expect("structs");
  let via_struct = trim_parse(body).expect("struct path");
  assert_eq!(
    serde_json::to_string(&via_struct).expect("serialize"),
    serde_json::to_string(&via_value).expect("serialize"),
    "the two paths disagree on: {body}"
  );
}

/// The bundled snapshot round-trips identically through both paths.
#[test]
fn trim_parse_matches_the_value_path_on_the_bundled_db() {
  assert_paths_agree(BUNDLED_DETECTABLE);
}

/// A body that parses but breaks a structural cap is rejected, on both
/// the direct and the trimmed path.
///
/// Measured against the real `applications/detectable` body (see
/// [`MAX_EXECUTABLES_PER_ENTRY`]): the largest values are 16 executables,
/// 8 aliases, 204-char names and 109-char executable names, and they are
/// identical in the bundled snapshot and in the live one. The caps sit
/// an order of magnitude above that, so ordinary growth never trips
/// them — but a body that parses is not automatically a body worth
/// building.
#[test]
fn structural_caps_reject_a_hostile_body() {
  let exes: String = (0..MAX_EXECUTABLES_PER_ENTRY + 1)
    .map(|i| format!(r#"{{"name":"e{i}.exe","os":"win32","is_launcher":false}}"#))
    .collect::<Vec<_>>()
    .join(",");
  let cases: [(&str, String); 7] = [
    (
      "too many executables in one entry",
      format!(r#"[{{"id":"1","name":"G","hook":false,"executables":[{exes}]}}]"#),
    ),
    (
      "too many aliases in one entry",
      format!(
        r#"[{{"id":"1","name":"G","hook":false,"aliases":[{}]}}]"#,
        (0..MAX_ALIASES_PER_ENTRY + 1)
          .map(|i| format!(r#""a{i}""#))
          .collect::<Vec<_>>()
          .join(",")
      ),
    ),
    (
      "too many entries",
      format!(
        "[{}]",
        (0..MAX_ENTRIES + 1)
          .map(|i| format!(r#"{{"id":"{i}","name":"G","hook":false}}"#))
          .collect::<Vec<_>>()
          .join(",")
      ),
    ),
    (
      "name past its cap",
      format!(
        r#"[{{"id":"1","hook":false,"name":"{}"}}]"#,
        "n".repeat(MAX_NAME_LEN + 1)
      ),
    ),
    (
      "executable name past its cap",
      format!(
        r#"[{{"id":"1","name":"G","hook":false,"executables":[{{"name":"{}","os":"linux","is_launcher":false}}]}}]"#,
        "e".repeat(MAX_EXECUTABLE_NAME_LEN + 1)
      ),
    ),
    (
      "application id past its cap",
      format!(
        r#"[{{"id":"{}","name":"G","hook":false}}]"#,
        "1".repeat(MAX_ID_LEN + 1)
      ),
    ),
    (
      "too many third-party SKUs in one entry",
      format!(
        r#"[{{"id":"1","name":"G","hook":false,"third_party_skus":[{}]}}]"#,
        (0..MAX_THIRD_PARTY_SKUS_PER_ENTRY + 1)
          .map(|i| format!(r#"{{"distributor":"steam","id":"{i}","sku":null}}"#))
          .collect::<Vec<_>>()
          .join(",")
      ),
    ),
  ];

  for (label, body) in cases {
    let direct = serde_json::from_str::<Vec<DetectableActivity>>(&body)
      .ok()
      .map(|entries| enforce_structure(&entries).is_err());
    let trimmed = trim_parse(&body).err().map(|_| ());
    assert_eq!(
      direct,
      Some(true),
      "{label}: the direct path must reject it (direct={direct:?})"
    );
    assert_eq!(
      trimmed,
      Some(()),
      "{label}: the trimmed path must reject it too"
    );
  }
}

/// The real database is accepted: the caps must not be the thing that
/// breaks a refresh. The bundled snapshot is a real body, so this is
/// the guard against a cap set below the data.
#[test]
fn structural_caps_accept_the_real_database() {
  let via_struct = trim_parse(BUNDLED_DETECTABLE).expect("the bundled database parses");
  enforce_structure(&via_struct).expect("the real database is within every cap");
}

/// A database shaped like the real one — every field at its measured
/// top end — is accepted.
///
/// The live `applications/detectable` body and the bundled snapshot
/// measure the same: 16 executables, 8 aliases, 31 Steam SKUs and a
/// 204-character name at the top of the distribution. This builds that
/// shape by hand rather than trusting a committed file, so a cap lowered
/// below the real data fails here even before the bundled snapshot is
/// refreshed.
#[test]
fn caps_admit_a_database_shaped_like_the_real_one() {
  let executables: String = (0..16)
    .map(|i| format!(r#"{{"name":"e{i}.exe","os":"linux","is_launcher":false}}"#))
    .collect::<Vec<_>>()
    .join(",");
  let aliases: String = (0..8)
    .map(|i| format!(r#""alias {i}""#))
    .collect::<Vec<_>>()
    .join(",");
  let skus: String = (0..31)
    .map(|i| format!(r#"{{"distributor":"steam","id":"{i}","sku":null}}"#))
    .collect::<Vec<_>>()
    .join(",");
  let body = format!(
    r#"[{{"id":"1045800378228281345","name":"{}","hook":false,
           "aliases":[{aliases}],"executables":[{executables}],
           "third_party_skus":[{skus}]}}]"#,
    "n".repeat(204)
  );

  parse_database(&body).expect("a database at the real top end is within every cap");
}

/// Defaults, empty arrays, empty aliases, extra keys, and the SKU
/// filtering rules all agree between the paths.
#[test]
fn trim_parse_matches_the_value_path_on_edge_shapes() {
  // Bare entries (defaults), null activity fields, extra keys dropped.
  assert_paths_agree(r#"[{"id":"1"}]"#);
  assert_paths_agree(r#"[{}]"#);
  assert_paths_agree(
    r#"[{"id":"2","name":"N","description":"dropped","developers":[{"id":"d","name":"D"}]}]"#,
  );

  // Empty aliases stay absent; blank aliases are dropped.
  assert_paths_agree(r#"[{"id":"3","aliases":[]}]"#);
  assert_paths_agree(r#"[{"id":"3","aliases":["  ",""]}]"#);
  assert_paths_agree(r#"[{"id":"3","aliases":["Alt","  "]}]"#);

  // Executables: absent, empty, and with arguments kept only when a
  // non-empty string.
  assert_paths_agree(r#"[{"id":"4","executables":[]}]"#);
  assert_paths_agree(r#"[{"id":"4","executables":[{}]}]"#);
  assert_paths_agree(r#"[{"id":"4","executables":[{"name":"g.exe","arguments":""}]}]"#);
  assert_paths_agree(r#"[{"id":"4","executables":[{"name":"g.exe","arguments":"--x"}]}]"#);

  // SKUs: empty distributor or id dropped; numeric id renders.
  assert_paths_agree(r#"[{"id":"5","third_party_skus":[]}]"#);
  assert_paths_agree(r#"[{"id":"5","third_party_skus":[{"distributor":"","id":"1"}]}]"#);
  assert_paths_agree(r#"[{"id":"5","third_party_skus":[{"distributor":"steam","id":"99"}]}]"#);
  assert_paths_agree(r#"[{"id":"5","third_party_skus":[{"distributor":"steam","id":99}]}]"#);

  // A wrongly-typed entry the projection tolerates: a number in aliases
  // is dropped, a non-object in executables is defaulted. The struct
  // path must tolerate it identically, which is why those arrays stay
  // untyped.
  assert_paths_agree(r#"[{"id":"6","aliases":[1,"Alt"],"executables":[7,{"name":"g.exe"}]}]"#);
}

/// Whatever the projection accepts or rejects, both paths agree — and
/// they are compared as whole paths (projection *and* final struct
/// parse), because `trim_detectable_value` only projects and its callers
/// run the parse themselves. A newly accepted or newly rejected body
/// would change which fetch a caller falls back from.
#[test]
fn trim_parse_agrees_on_acceptance_not_just_on_content() {
  for body in [
    r#"[{"id":7}]"#,
    r#"[{"id":null}]"#,
    r#"[{"id":"ok"}]"#,
    r#"[{"name":7}]"#,
    r#"[{"name":null}]"#,
    r#"[{"hook":"yes"}]"#,
    r#"[{"hook":null}]"#,
    r#"[{"hook":true}]"#,
    r#"[{"aliases":[1,"Alt"],"executables":[7,{"name":"g.exe"}]}]"#,
    r#"{"not":"an array"}"#,
    r#"not json{{"#,
    r#"[]"#,
    r#"[{"id":"1","description":"d","developers":[{"id":"x","name":"y"}]}]"#,
  ] {
    let value = trim_detectable_value(body).and_then(|projected| {
      serde_json::from_value::<Vec<DetectableActivity>>(projected)
        .map_err(|err| adsum_protocol::error::AdsumError::InvalidJson { source: err })
    });
    let structured = trim_parse(body);
    assert_eq!(
      value.is_ok(),
      structured.is_ok(),
      "acceptance disagrees for {body}: value={:?} struct={:?}",
      value.map(|v| v.len()).map_err(|e| e.to_string()),
      structured.map(|v| v.len()).map_err(|e| e.to_string()),
    );
  }
}
