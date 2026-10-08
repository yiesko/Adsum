//! Transport tuning: port range, feature flags, queue bounds.

/// Default bound for the transport→bridge event queue.
///
/// Deep enough for legitimate bursts (100+ games publishing at once is
/// unrealistic; floods are shed counted past this). Paired with the
/// `rsrpc-ws` global queue bound, this keeps the transport→bridge leg to
/// a few dozen in-flight commands instead of a thousand.
pub const DEFAULT_EVENT_QUEUE: usize = 64;

/// Default cap on concurrent game connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 32;

/// Default bound per game-client outbox.
pub const DEFAULT_PER_CLIENT_QUEUE: usize = 8;

/// Game WebSocket transport configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WsTransportConfig {
  /// First port of the loopback bind range (inclusive).
  pub port_start: u16,
  /// Last port of the loopback bind range (inclusive).
  pub port_end: u16,
  /// Forward `SET_ACTIVITY` to the sink (game presence path).
  pub set_activity: bool,
  /// Forward secondary commands (browser/deep-link/callbacks).
  pub secondary_events: bool,
  /// Bound for the transport→bridge event queue.
  pub event_queue: usize,
  /// Cap on concurrent game connections.
  pub max_connections: usize,
  /// Bound per game-client outbox.
  pub per_client_queue: usize,
  /// Cap for a single incoming message; `None` disables (not recommended).
  ///
  /// Defaults to the `rsrpc-ws` bound so the transport inherits the
  /// server's budget unless it deliberately overrides it.
  pub max_message_size: Option<usize>,
  /// Cap for a single incoming frame; `None` disables (not recommended).
  pub max_frame_size: Option<usize>,
}

impl WsTransportConfig {
  /// Bind range `port_start..=port_end` on loopback with full behavior.
  /// Use `(0, 0)` for an OS-assigned ephemeral port (tests).
  #[must_use]
  pub fn new(port_start: u16, port_end: u16) -> Self {
    Self {
      port_start,
      port_end,
      set_activity: true,
      secondary_events: true,
      event_queue: DEFAULT_EVENT_QUEUE,
      max_connections: DEFAULT_MAX_CONNECTIONS,
      per_client_queue: DEFAULT_PER_CLIENT_QUEUE,
      max_message_size: Some(rsrpc_ws::config::DEFAULT_MAX_MESSAGE_SIZE),
      max_frame_size: Some(rsrpc_ws::config::DEFAULT_MAX_FRAME_SIZE),
    }
  }

  /// Forward `SET_ACTIVITY` to the sink.
  #[must_use]
  pub fn set_activity(mut self, value: bool) -> Self {
    self.set_activity = value;
    self
  }

  /// Forward secondary commands (browser/deep-link/callbacks).
  #[must_use]
  pub fn secondary_events(mut self, value: bool) -> Self {
    self.secondary_events = value;
    self
  }

  /// Bound for the transport→bridge event queue. Must be non-zero.
  #[must_use]
  pub fn event_queue(mut self, n: usize) -> Self {
    self.event_queue = n;
    self
  }

  /// Cap on concurrent game connections. Must be non-zero.
  #[must_use]
  pub fn max_connections(mut self, n: usize) -> Self {
    self.max_connections = n;
    self
  }

  /// Bound per game-client outbox. Must be non-zero.
  #[must_use]
  pub fn per_client_queue(mut self, n: usize) -> Self {
    self.per_client_queue = n;
    self
  }

  /// Cap for a single incoming message; `None` disables the cap.
  #[must_use]
  pub fn max_message_size(mut self, n: Option<usize>) -> Self {
    self.max_message_size = n;
    self
  }

  /// Cap for a single incoming frame; `None` disables the cap.
  #[must_use]
  pub fn max_frame_size(mut self, n: Option<usize>) -> Self {
    self.max_frame_size = n;
    self
  }
}

#[cfg(test)]
mod tests {
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
      Some(rsrpc_ws::config::DEFAULT_MAX_MESSAGE_SIZE)
    );
    assert_eq!(
      config.max_frame_size,
      Some(rsrpc_ws::config::DEFAULT_MAX_FRAME_SIZE)
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
  /// silently falling back to `rsrpc-ws` defaults: a caller that tunes
  /// `event_queue` must see it honored at the socket.
  #[test]
  fn queue_bounds_reach_the_server_config() {
    let config = WsTransportConfig::new(0, 0)
      .event_queue(7)
      .max_connections(9)
      .per_client_queue(3);
    let ws = rsrpc_ws::ServerConfig::builder("127.0.0.1:0".parse().unwrap())
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
}
