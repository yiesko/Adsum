use super::*;

/// Each daemon flag disables only its own command class; disabling both
/// is expressed by not binding at all (see `start_scanner`).
#[test]
fn ws_config_maps_each_connector_flag_to_its_class() {
  let cfg = ws_transport_config(&RPCConfig::default());
  assert!(cfg.set_activity);
  assert!(cfg.secondary_events);

  let no_secondary = RPCConfig {
    enable_secondary_events: false,
    ..RPCConfig::default()
  };
  let cfg = ws_transport_config(&no_secondary);
  assert!(cfg.set_activity);
  assert!(!cfg.secondary_events);

  let no_ws = RPCConfig {
    enable_websocket_connector: false,
    ..RPCConfig::default()
  };
  let cfg = ws_transport_config(&no_ws);
  assert!(!cfg.set_activity);
  assert!(cfg.secondary_events);
}

/// `--state-file` routes bridge snapshots to the temp-dir slots; the
/// default keeps snapshots off so ordinary runs never write them.
#[test]
fn state_file_routes_bridge_snapshots_to_tmpdir() {
  let off = bridge_config_for(&RPCConfig::default(), None, None);
  assert!(off.state_dir.is_none());

  let on = bridge_config_for(
    &RPCConfig {
      state_file: true,
      ..RPCConfig::default()
    },
    None,
    None,
  );
  assert_eq!(on.state_dir, Some(std::env::temp_dir()));
}
