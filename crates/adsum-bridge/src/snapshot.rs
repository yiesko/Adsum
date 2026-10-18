//! Snapshot persistence: flatten the replay cache into state files.
//!
//! Snapshots persist dirty-gated on a cadence (never per publish) into
//! the configured state dir, so external tooling can read presence
//! without joining the bridge.
use super::bridge::Shared;
use super::replay::ReplayCache;
use super::state::{StateActivity, StateServer, StateServers, StateSnapshot, write_snapshot};
use std::borrow::Cow;

/// Loopback host every bridge front-end is recorded under in a snapshot.
const LOOPBACK_HOST: &str = "127.0.0.1";

/// The snapshot's server table: the three bridge front-ends, all on
/// loopback, plus the IPC socket path when the transport is up.
///
/// Extracted so the host choice is testable: it borrows the constant, so
/// rebuilding a snapshot per persist allocates nothing for it. The ports
/// come from the caller, as does the IPC path (the one value that is owned
/// here, because it is not a constant).
pub fn state_servers(json_port: u16, msgpack_port: u16, ws_port: Option<u16>) -> StateServers {
  StateServers {
    bridge: Some(StateServer {
      host: Cow::Borrowed(LOOPBACK_HOST),
      port: json_port,
    }),
    msgpack: Some(StateServer {
      host: Cow::Borrowed(LOOPBACK_HOST),
      port: msgpack_port,
    }),
    websocket: ws_port.map(|port| StateServer {
      host: Cow::Borrowed(LOOPBACK_HOST),
      port,
    }),
    ipc: None,
  }
}

impl Shared {
  /// Write the snapshot when dirty (spawn_blocking: sync temp+rename+fsync
  /// must not sit on an async worker). Best-effort: failures stay in
  /// diagnostics so a full tmpfs never breaks presence.
  pub(crate) async fn persist_now(&self) {
    let Some(path) = self.state_path.clone() else {
      return;
    };
    let mut servers = state_servers(self.json_port, self.msgpack_port, self.ws_port);
    servers.ipc = self.ipc_path.clone();
    let activities = state_activities(&self.cache.lock().unwrap_or_else(|e| e.into_inner()));
    let snapshot = StateSnapshot::new(&self.app_version, servers, activities);
    let result = tokio::task::spawn_blocking(move || write_snapshot(&path, &snapshot)).await;
    match result {
      Ok(Ok(())) => {}
      Ok(Err(err)) => tracing::debug!("[bridge] State snapshot failed: {err}"),
      Err(err) => tracing::debug!("[bridge] Snapshot task failed: {err}"),
    }
  }
}

/// Flatten the replay cache into state-snapshot activities (best-effort:
/// unparseable entries contribute their socket id only).
pub(crate) fn state_activities(cache: &ReplayCache) -> Vec<StateActivity> {
  let mut out = Vec::with_capacity(cache.len());
  out.extend(cache.iter().map(|(socket_id, (payload, _))| {
    let body: serde_json::Value =
      serde_json::from_str(&payload.json).unwrap_or(serde_json::Value::Null);
    let activity = body.get("activity");
    StateActivity {
      socket_id: socket_id.to_string(),
      name: activity
        .and_then(|item| item.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string),
      application_id: activity
        .and_then(|item| item.get("application_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string),
      // Recorded at build time, not parsed back out of the body. The other
      // three fields still come from the JSON: they live only there, and
      // duplicating them per cache entry to spare a parse on a
      // 5-second-cadence diagnostic would trade memory for trivia.
      pid: (payload.pid != 0).then_some(payload.pid),
      start_time: activity
        .and_then(|item| item.get("timestamps"))
        .and_then(|item| item.get("start"))
        .map(|value| match value {
          serde_json::Value::String(text) => text.clone(),
          other => other.to_string(),
        }),
      // Provenance stamped by the payload builders (generic matcher
      // source, or "sdk" for client cards); unclassified cards omit it.
      detection_source: payload
        .provenance
        .as_ref()
        .map(|provenance| provenance.source.clone()),
      detect_latency_ms: payload
        .provenance
        .as_ref()
        .and_then(|provenance| provenance.latency_ms),
    }
  }));
  out
}
