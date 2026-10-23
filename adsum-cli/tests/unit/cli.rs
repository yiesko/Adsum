use super::*;

/// `--state-file` (env `ADSUM_STATE_FILE`) enables presence snapshots;
/// the default keeps them off.
#[test]
fn state_file_flag_parses() {
  let on = Args::try_parse_from(["adsum-cli", "--state-file"]).expect("--state-file parses");
  assert!(on.state_file);
  let off = Args::try_parse_from(["adsum-cli"]).expect("defaults parse");
  assert!(!off.state_file);
}

/// `--bridge-allowed-origins` (env `ADSUM_BRIDGE_ALLOWED_ORIGINS`)
/// parses into a trimmed, blank-free list; unset stays empty.
#[test]
fn bridge_allowed_origins_flag_parses() {
  let on = Args::try_parse_from([
    "adsum-cli",
    "--bridge-allowed-origins",
    "https://a.example, https://b.example,,",
  ])
  .expect("--bridge-allowed-origins parses");
  assert_eq!(
    parse_csv_list(on.bridge_allowed_origins.as_deref()),
    vec!["https://a.example", "https://b.example"]
  );
  let off = Args::try_parse_from(["adsum-cli"]).expect("defaults parse");
  assert!(off.bridge_allowed_origins.is_none());
}

/// One entry shaped like the real database — every field at its measured
/// top end (16 executables, 8 aliases, 31 SKUs, a 204-character name).
fn real_shaped_entry() -> String {
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
  format!(
    r#"{{"id":"1045800378228281345","name":"{}","hook":false,
           "aliases":[{aliases}],"executables":[{executables}],
           "third_party_skus":[{skus}]}}"#,
    "n".repeat(204)
  )
}

/// A fetched body this daemon should never build — one entry past the
/// entry cap — must be refused at boot, the same way an unparseable body
/// is: the bundled snapshot comes up, not the fetched one.
///
/// The direct decode of this body succeeds, so only the structural caps
/// can refuse it. A boot path that decodes straight into the daemon
/// hands the automaton a database no other path in the process would
/// accept. The entries are minimal on purpose: what breaks this cap is
/// the count, and 100k fat entries would only buy the test a slow build.
#[test]
fn an_over_cap_fetched_body_falls_back_to_bundled() {
  let body = format!(
    "[{}]",
    vec![r#"{"id":"1","name":"n"}"#; adsum_detect::db::MAX_ENTRIES + 1].join(",")
  );
  let (_daemon, label) = daemon_from_fetched(body, RPCConfig::default())
    .expect("boot answers even when the body is refused");
  assert_eq!(
    label, "bundled-fallback",
    "an over-cap body must not be built into the daemon"
  );
}

/// The same parser, fed the real shape, still builds from the fetch: the
/// caps are a ceiling for a pathological database, not a bar for one that
/// looks like the live endpoint.
#[test]
fn a_real_shaped_fetched_body_still_builds_directly() {
  let body = format!("[{},{}]", real_shaped_entry(), real_shaped_entry());
  let (_daemon, label) =
    daemon_from_fetched(body, RPCConfig::default()).expect("a real-shaped body parses");
  assert_eq!(
    label, "fetched-direct",
    "the caps must not refuse a database shaped like the real one"
  );
}
