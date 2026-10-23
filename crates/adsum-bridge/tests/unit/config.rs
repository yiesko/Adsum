use super::*;

/// Default port ranges stay on the arRPC-known values.
#[test]
fn defaults_match_arrpc_ports() {
  let config = BridgeConfig::new(
    DEFAULT_JSON_PORT_START,
    DEFAULT_JSON_PORT_END,
    DEFAULT_MSGPACK_PORT_START,
    DEFAULT_MSGPACK_PORT_END,
  );
  assert_eq!(config.json_port_start, 1337);
  assert_eq!(config.msgpack_port_start, 1338);
  assert_eq!(config.persist_interval, DEFAULT_PERSIST_INTERVAL);
  assert!(config.state_dir.is_none());
}

/// The builder stores canonical origins, so the per-connection check
/// stays an exact comparison.
#[test]
fn allowed_origins_normalize_once() {
  let config = BridgeConfig::new(0, 0, 0, 0)
    .allowed_origins(vec!["HTTPS://My-Client.Example:443/".to_string()]);
  assert_eq!(
    config.allowed_origins,
    vec!["https://my-client.example".to_string()]
  );
}

/// The bridge's own bounds reach the server; unset fields keep the
/// `adsum-ws` default rather than uncapping anything.
#[test]
fn bridge_bounds_reach_the_server_config() {
  let bounds = BridgeConfig::new(0, 0, 0, 0)
    .max_connections(12)
    .event_queue(34)
    .per_client_queue(5)
    .max_message_size(2048)
    .max_frame_size(1024)
    .server_bounds();

  let cfg = bounds
    .apply(adsum_ws::ServerConfig::builder(
      "127.0.0.1:0".parse().unwrap(),
    ))
    .build()
    .unwrap();
  assert_eq!(cfg.max_connections, 12);
  assert_eq!(cfg.event_queue, 34);
  assert_eq!(cfg.per_client_queue, 5);
  assert_eq!(cfg.max_message_size, Some(2048));
  assert_eq!(cfg.max_frame_size, Some(1024));
}

/// A bare config leaves every bound at the server default.
#[test]
fn bridge_bounds_inherit_server_defaults() {
  let cfg = BridgeConfig::new(0, 0, 0, 0)
    .server_bounds()
    .apply(adsum_ws::ServerConfig::builder(
      "127.0.0.1:0".parse().unwrap(),
    ))
    .build()
    .unwrap();
  assert_eq!(
    cfg.max_connections,
    adsum_ws::config::DEFAULT_MAX_CONNECTIONS
  );
  assert_eq!(cfg.event_queue, adsum_ws::config::DEFAULT_EVENT_QUEUE);
  assert_eq!(
    cfg.per_client_queue,
    adsum_ws::config::DEFAULT_PER_CLIENT_QUEUE
  );
  assert_eq!(
    cfg.max_message_size,
    Some(adsum_ws::config::DEFAULT_MAX_MESSAGE_SIZE)
  );
  assert_eq!(
    cfg.max_frame_size,
    Some(adsum_ws::config::DEFAULT_MAX_FRAME_SIZE)
  );
}
