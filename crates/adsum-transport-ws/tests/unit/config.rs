use super::*;

/// Defaults enable activity plus secondary events with bounded queues.
#[test]
fn defaults_enable_full_behavior() {
  let config = WsTransportConfig::new(6463, 6472);
  assert!(config.set_activity);
  assert!(config.secondary_events);
  assert_eq!(config.event_queue, DEFAULT_EVENT_QUEUE);
  assert_eq!(config.max_connections, DEFAULT_MAX_CONNECTIONS);
  assert_eq!(config.per_client_queue, DEFAULT_PER_CLIENT_QUEUE);
  // Size bounds default to the server's own budget, so the transport
  // inherits them unless it overrides explicitly.
  assert_eq!(
    config.max_message_size,
    Some(adsum_ws::config::DEFAULT_MAX_MESSAGE_SIZE)
  );
  assert_eq!(
    config.max_frame_size,
    Some(adsum_ws::config::DEFAULT_MAX_FRAME_SIZE)
  );
}

/// Size bounds survive the builder, including the `None` (disabled) form.
#[test]
fn size_bounds_are_settable() {
  let config = WsTransportConfig::new(0, 0)
    .max_message_size(Some(4096))
    .max_frame_size(Some(1024));
  assert_eq!(config.max_message_size, Some(4096));
  assert_eq!(config.max_frame_size, Some(1024));

  let uncapped = WsTransportConfig::new(0, 0)
    .max_message_size(None)
    .max_frame_size(None);
  assert_eq!(uncapped.max_message_size, None);
  assert_eq!(uncapped.max_frame_size, None);
}

/// The transport's queue bounds are propagated to the server rather than
/// silently falling back to `adsum-ws` defaults: a caller that tunes
/// `event_queue` must see it honored at the socket.
#[test]
fn queue_bounds_reach_the_server_config() {
  let config = WsTransportConfig::new(0, 0)
    .event_queue(7)
    .max_connections(9)
    .per_client_queue(3);
  let ws = adsum_ws::ServerConfig::builder("127.0.0.1:0".parse().unwrap())
    .max_connections(config.max_connections)
    .event_queue(config.event_queue)
    .per_client_queue(config.per_client_queue)
    .max_message_size(config.max_message_size)
    .max_frame_size(config.max_frame_size)
    .build()
    .unwrap();
  assert_eq!(ws.event_queue, 7);
  assert_eq!(ws.max_connections, 9);
  assert_eq!(ws.per_client_queue, 3);
  assert_eq!(ws.max_message_size, config.max_message_size);
  assert_eq!(ws.max_frame_size, config.max_frame_size);
}
