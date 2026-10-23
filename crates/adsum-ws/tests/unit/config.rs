use super::*;

/// Ephemeral loopback address for hermetic bind tests.
fn loopback() -> SocketAddr {
  "127.0.0.1:0".parse().unwrap()
}

/// Defaults match the documented production bounds.
#[test]
fn builder_defaults_are_sane() {
  let cfg = ServerConfig::builder(loopback()).build().unwrap();
  assert_eq!(cfg.max_connections, DEFAULT_MAX_CONNECTIONS);
  assert_eq!(cfg.event_queue, DEFAULT_EVENT_QUEUE);
  assert_eq!(cfg.per_client_queue, DEFAULT_PER_CLIENT_QUEUE);
  assert_eq!(cfg.max_message_size, Some(DEFAULT_MAX_MESSAGE_SIZE));
  assert_eq!(cfg.max_frame_size, Some(DEFAULT_MAX_FRAME_SIZE));
  assert_eq!(cfg.keepalive_interval, DEFAULT_KEEPALIVE_INTERVAL);
  assert_eq!(cfg.idle_timeout, DEFAULT_IDLE_TIMEOUT);
}

/// Zero bounds are rejected at build time, never at bind time.
#[test]
fn builder_rejects_zero_bounds() {
  assert!(matches!(
    ServerConfig::builder(loopback()).max_connections(0).build(),
    Err(Error::Config(_))
  ));
  assert!(matches!(
    ServerConfig::builder(loopback()).event_queue(0).build(),
    Err(Error::Config(_))
  ));
  assert!(matches!(
    ServerConfig::builder(loopback())
      .per_client_queue(0)
      .build(),
    Err(Error::Config(_))
  ));
  assert!(matches!(
    ServerConfig::builder(loopback())
      .keepalive_interval(Duration::ZERO)
      .build(),
    Err(Error::Config(_))
  ));
  assert!(matches!(
    ServerConfig::builder(loopback())
      .idle_timeout(Duration::ZERO)
      .build(),
    Err(Error::Config(_))
  ));
}
