//! Bridge tuning: port ranges, snapshot extras, cadences.

use std::path::PathBuf;
use std::time::Duration;

/// Default JSON bridge range (arRPC-compatible).
pub const DEFAULT_JSON_PORT_START: u16 = 1337;
/// Default JSON bridge range end.
pub const DEFAULT_JSON_PORT_END: u16 = 1347;
/// Default MessagePack bridge port.
pub const DEFAULT_MSGPACK_PORT_START: u16 = 1338;
/// Default MessagePack bridge range end.
pub const DEFAULT_MSGPACK_PORT_END: u16 = 1348;

/// Default snapshot cadence: presence writes land at most this often no
/// matter the publish rate (the legacy wrote on *every* publish).
pub const DEFAULT_PERSIST_INTERVAL: Duration = Duration::from_secs(5);

/// Default replay rebroadcast cadence for clients that missed a frame.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Cap on replayed activities (arRPC keeps 50): bounds memory when many
/// distinct pids publish without clearing.
pub const MAX_CACHED_ACTIVITIES: usize = 50;

/// arRPC-compatible activity bridge configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BridgeConfig {
  /// First JSON port to try (inclusive).
  pub json_port_start: u16,
  /// Last JSON port to try (inclusive).
  pub json_port_end: u16,
  /// First MessagePack port to try (inclusive).
  pub msgpack_port_start: u16,
  /// Last MessagePack port to try (inclusive).
  pub msgpack_port_end: u16,
  /// Version stamped into state snapshots (the daemon version; defaults
  /// to this crate's version when the wiring does not override it).
  pub app_version: String,
  /// Directory holding the snapshot slot (`None` disables snapshots).
  pub state_dir: Option<PathBuf>,
  /// Game WebSocket port, recorded in the snapshot for tooling.
  pub ws_port: Option<u16>,
  /// IPC socket path, recorded in the snapshot for tooling.
  pub ipc_path: Option<String>,
  /// Extra browser origins allowed to drive bridge commands, beyond
  /// Discord's own pages (`ADSUM_BRIDGE_ALLOWED_ORIGINS`, comma-separated).
  /// Absent `Origin` (native clients) always passes.
  pub allowed_origins: Vec<String>,
  /// Snapshot write cadence (dirty-gated).
  pub persist_interval: Duration,
  /// Replay rebroadcast cadence.
  pub refresh_interval: Duration,
  /// Cap on concurrent consumers per server; `None` inherits the `adsum-ws`
  /// default.
  pub max_connections: Option<usize>,
  /// Bound for each server's global event queue; `None` inherits the
  /// `adsum-ws` default.
  pub event_queue: Option<usize>,
  /// Bound for each consumer's outbound queue; `None` inherits the
  /// `adsum-ws` default.
  pub per_client_queue: Option<usize>,
  /// Cap for a single incoming message; `None` inherits the `adsum-ws`
  /// default. Passing `Some(None)` is not expressible here: an uncapped
  /// message is not recommended for a bridge.
  pub max_message_size: Option<usize>,
  /// Cap for a single incoming frame; `None` inherits the `adsum-ws`
  /// default.
  pub max_frame_size: Option<usize>,
}

impl BridgeConfig {
  /// Port ranges with arRPC-compatible defaults otherwise.
  /// `(0, 0, 0, 0)` binds ephemeral ports (tests).
  #[must_use]
  pub fn new(
    json_port_start: u16,
    json_port_end: u16,
    msgpack_port_start: u16,
    msgpack_port_end: u16,
  ) -> Self {
    Self {
      json_port_start,
      json_port_end,
      msgpack_port_start,
      msgpack_port_end,
      app_version: env!("CARGO_PKG_VERSION").to_string(),
      state_dir: None,
      ws_port: None,
      ipc_path: None,
      allowed_origins: Vec::new(),
      persist_interval: DEFAULT_PERSIST_INTERVAL,
      refresh_interval: DEFAULT_REFRESH_INTERVAL,
      max_connections: None,
      event_queue: None,
      per_client_queue: None,
      max_message_size: None,
      max_frame_size: None,
    }
  }

  /// Version stamped into state snapshots.
  #[must_use]
  pub fn app_version(mut self, version: impl Into<String>) -> Self {
    self.app_version = version.into();
    self
  }

  /// Enable snapshots in `dir` (slot selected at bind).
  #[must_use]
  pub fn state_dir(mut self, dir: PathBuf) -> Self {
    self.state_dir = Some(dir);
    self
  }

  /// Game WebSocket port recorded in the snapshot.
  #[must_use]
  pub fn ws_port(mut self, port: Option<u16>) -> Self {
    self.ws_port = port;
    self
  }

  /// IPC socket path recorded in the snapshot.
  #[must_use]
  pub fn ipc_path(mut self, path: Option<String>) -> Self {
    self.ipc_path = path;
    self
  }

  /// Extra browser origins allowed to drive bridge commands.
  /// Normalized once (lowercase, no trailing slash, no default port)
  /// so configured values match browser serialization.
  #[must_use]
  pub fn allowed_origins(mut self, origins: Vec<String>) -> Self {
    self.allowed_origins = origins
      .iter()
      .map(|origin| super::origin::normalize_origin(origin))
      .collect();
    self
  }

  /// Snapshot write cadence. Must be non-zero.
  #[must_use]
  pub fn persist_interval(mut self, interval: Duration) -> Self {
    self.persist_interval = interval;
    self
  }

  /// Replay rebroadcast cadency. Must be non-zero.
  #[must_use]
  pub fn refresh_interval(mut self, interval: Duration) -> Self {
    self.refresh_interval = interval;
    self
  }

  /// Cap on concurrent consumers per server.
  #[must_use]
  pub fn max_connections(mut self, n: usize) -> Self {
    self.max_connections = Some(n);
    self
  }

  /// Bound for each server's global event queue.
  #[must_use]
  pub fn event_queue(mut self, n: usize) -> Self {
    self.event_queue = Some(n);
    self
  }

  /// Bound for each consumer's outbound queue.
  #[must_use]
  pub fn per_client_queue(mut self, n: usize) -> Self {
    self.per_client_queue = Some(n);
    self
  }

  /// Cap for a single incoming message.
  #[must_use]
  pub fn max_message_size(mut self, n: usize) -> Self {
    self.max_message_size = Some(n);
    self
  }

  /// Cap for a single incoming frame.
  #[must_use]
  pub fn max_frame_size(mut self, n: usize) -> Self {
    self.max_frame_size = Some(n);
    self
  }

  /// Queue and size budget the bridge hands to both servers. `None` fields
  /// fall back to the `adsum-ws` defaults, which are already a conservative
  /// loopback budget.
  pub(crate) fn server_bounds(&self) -> ServerBounds {
    ServerBounds {
      max_connections: self.max_connections,
      event_queue: self.event_queue,
      per_client_queue: self.per_client_queue,
      max_message_size: self.max_message_size,
      max_frame_size: self.max_frame_size,
    }
  }
}

/// Queue and size budget applied to both bridge servers.
///
/// Every field is optional so an unset value inherits the `adsum-ws`
/// default rather than silently uncapping the server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ServerBounds {
  max_connections: Option<usize>,
  event_queue: Option<usize>,
  per_client_queue: Option<usize>,
  max_message_size: Option<usize>,
  max_frame_size: Option<usize>,
}

impl ServerBounds {
  /// Apply the overrides to a builder, leaving unset fields at their
  /// default.
  #[must_use]
  pub(crate) fn apply(
    self,
    builder: adsum_ws::ServerConfigBuilder,
  ) -> adsum_ws::ServerConfigBuilder {
    let builder = match self.max_connections {
      Some(n) => builder.max_connections(n),
      None => builder,
    };
    let builder = match self.event_queue {
      Some(n) => builder.event_queue(n),
      None => builder,
    };
    let builder = match self.per_client_queue {
      Some(n) => builder.per_client_queue(n),
      None => builder,
    };
    let builder = match self.max_message_size {
      Some(n) => builder.max_message_size(Some(n)),
      None => builder,
    };
    match self.max_frame_size {
      Some(n) => builder.max_frame_size(Some(n)),
      None => builder,
    }
  }
}

#[cfg(test)]
#[path = "../tests/unit/config.rs"]
mod tests;
