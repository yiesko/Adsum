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

fn test_config() -> RPCConfig {
  RPCConfig {
    port: 0,
    bridge_port_end: 0,
    msgpack_port: 0,
    ws_port_start: 0,
    ws_port_end: 0,
    enable_process_scanner: false,
    enable_proc_events: false,
    enable_db_update: false,
    enable_ipc_connector: false,
    ..RPCConfig::default()
  }
}

#[tokio::test]
async fn websocket_bind_failure_tears_down_promptly() {
  let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
  let port = held.local_addr().unwrap().port();
  let config = RPCConfig {
    ws_port_start: port,
    ws_port_end: port,
    ..test_config()
  };
  let daemon = Daemon::from_parsed(Vec::new(), config);
  let error = tokio::time::timeout(
    std::time::Duration::from_secs(5),
    daemon.run_until(async {}),
  )
  .await
  .expect("bind failure tears down promptly")
  .expect_err("held WebSocket port must fail");
  assert!(matches!(error, AdsumError::WsBind { .. }));
}

#[tokio::test]
async fn bridge_bind_failure_tears_down_promptly() {
  let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
  let port = held.local_addr().unwrap().port();
  let config = RPCConfig {
    port,
    bridge_port_end: port,
    ..test_config()
  };
  let daemon = Daemon::from_parsed(Vec::new(), config);
  tokio::time::timeout(
    std::time::Duration::from_secs(5),
    daemon.run_until(async {}),
  )
  .await
  .expect("bridge bind failure tears down promptly")
  .expect_err("held bridge port must fail");
}
