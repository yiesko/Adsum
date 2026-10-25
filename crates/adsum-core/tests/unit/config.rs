use super::*;

/// Defaults keep the legacy ports and every connector enabled.
#[test]
fn defaults_match_legacy_ports_and_flags() {
  let config = RPCConfig::default();
  assert!(config.enable_process_scanner);
  assert!(config.enable_proc_events);
  assert!(config.enable_ipc_connector);
  assert!(config.enable_websocket_connector);
  assert!(config.enable_secondary_events);
  assert_eq!(config.port, 1337);
  assert_eq!(config.bridge_port_end, 1347);
  assert_eq!(config.msgpack_port, 1338);
  assert_eq!(config.ws_port_start, 6463);
  assert_eq!(config.ws_port_end, 6472);
  assert_eq!(config.scan_interval_secs, 5);
  assert!(!config.enable_db_update);
  assert!(config.ignored_ids.is_empty());
  assert!(!config.state_file);
}

/// The builder reaches every field (no silent fallback).
#[test]
fn builder_covers_every_field() {
  let config = RPCConfig::builder()
    .enable_process_scanner(false)
    .enable_proc_events(false)
    .enable_ipc_connector(false)
    .enable_websocket_connector(false)
    .enable_secondary_events(false)
    .port(1)
    .bridge_port_end(2)
    .msgpack_port(3)
    .ws_port_start(4)
    .ws_port_end(5)
    .scan_interval_secs(6)
    .db_url(Some("https://example.invalid/db".to_string()))
    .enable_db_update(true)
    .initial_db_etag(Some("tag".to_string()))
    .initial_db_content_hash(Some((1, 2)))
    .ignored_ids(vec!["7".to_string()])
    .exclusions_url(Some("https://example.invalid/ex".to_string()))
    .app_version("test".to_string())
    .state_file(true)
    .build();
  assert!(!config.enable_process_scanner);
  assert_eq!(config.port, 1);
  assert_eq!(config.msgpack_port, 3);
  assert_eq!(config.initial_db_content_hash, Some((1, 2)));
  assert_eq!(config.ignored_ids, vec!["7".to_string()]);
  assert_eq!(config.app_version, "test");
  assert!(config.state_file);
}
