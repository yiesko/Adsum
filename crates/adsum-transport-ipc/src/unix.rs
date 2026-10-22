//! Unix IPC server: async accept loop, blocking pumps in a [`JoinSet`].
//!
//! The listener isTokio-native (no 50ms poll sleep); each connection runs
//! `handle_stream` in `spawn_blocking` (blocking socket I/O must never sit
//! on an async worker). Shutdown cancels the accept loop, drains the set
//! with a deadline, then aborts stragglers.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use adsum_protocol::error::{AdsumError, Result};
use adsum_types::AppId;
use adsum_types::cmd::ActivityCmd;
use adsum_types::user::RpcUser;
use serde_json::Value;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::frame::{IpcFacilitator, handle_stream};
use crate::paths::{
  fanout_socket_link, preferred_socket_dir_from, prepare_socket_dir, remove_socket_links,
  socket_alias_dirs, socket_dir_candidates, socket_file_name,
};
use crate::probe::socket_holder_alive;
use crate::sink::{DEFAULT_IPC_QUEUE, EventSink};

/// Upper bound for graceful connection drain in [`IpcTransport::shutdown`].
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Backoff after a listener accept error: transient per-connection
/// failures retry at once, persistent ones (fd exhaustion) pause briefly
/// instead of hot-spinning. Duplicated from `adsum-ws` (peer crates share
/// no helper crate for this); both copies are pinned by the unit test below.
fn accept_error_backoff(err: &std::io::Error) -> Option<Duration> {
  match err.kind() {
    ErrorKind::ConnectionAborted | ErrorKind::Interrupted => None,
    _ => Some(Duration::from_millis(50)),
  }
}

/// Per-connection protocol state plus shared handles.
struct ConnFacilitator {
  handshake: bool,
  client_id: AppId,
  pid: u64,
  nonce: Value,
  user: Arc<Mutex<RpcUser>>,
  sink: EventSink,
  /// Pids published on this connection, oldest first (bounded).
  published_pids: Vec<u64>,
}

impl ConnFacilitator {
  /// Blank per-connection state (handshake pending, no pid yet).
  fn fresh(user: Arc<Mutex<RpcUser>>, sink: EventSink) -> Self {
    Self {
      handshake: false,
      client_id: AppId::from(""),
      pid: 0,
      nonce: Value::Null,
      user,
      sink,
      published_pids: Vec::new(),
    }
  }
}

impl IpcFacilitator for ConnFacilitator {
  /// Whether the handshake completed on this connection.
  fn handshake(&self) -> bool {
    self.handshake
  }
  /// Record handshake completion.
  fn set_handshake(&mut self, handshake: bool) {
    self.handshake = handshake;
  }
  /// Application id from the handshake (empty until then).
  fn client_id(&self) -> AppId {
    self.client_id.clone()
  }
  /// Store the handshake application id.
  fn set_client_id(&mut self, client_id: AppId) {
    self.client_id = client_id;
  }
  /// Last pid seen on this connection (0 until the first activity).
  fn pid(&self) -> u64 {
    self.pid
  }
  /// Store the latest activity pid.
  fn set_pid(&mut self, pid: u64) {
    self.pid = pid;
  }
  /// Latest command nonce (lock-step replies echo it back).
  fn nonce(&self) -> Value {
    self.nonce.clone()
  }
  /// Store the latest command nonce.
  fn set_nonce(&mut self, nonce: Value) {
    self.nonce = nonce;
  }
  /// Current identity rendered as the READY payload.
  fn user_payload(&self) -> String {
    self
      .user
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .ready_payload()
  }
  /// Snapshot of the current identity for `GET_USER` answers.
  fn current_user(&self) -> RpcUser {
    self.user.lock().unwrap_or_else(|e| e.into_inner()).clone()
  }
  /// Downstream sink for validated commands and clears.
  fn sink(&self) -> &EventSink {
    &self.sink
  }
  /// Remember a published pid for disconnect cleanup (bounded history).
  fn note_published_pid(&mut self, pid: u64) {
    crate::frame::track_pid(&mut self.published_pids, pid);
  }
  /// Admit before forwarding: unknown pids past the bound are refused so
  /// every forwarded pid stays covered by disconnect cleanup.
  fn admit_published_pid(&mut self, pid: u64) -> bool {
    crate::frame::admit_pid(&mut self.published_pids, pid)
  }
  /// Drain the published-pid history for disconnect clears.
  fn take_published_pids(&mut self) -> Vec<u64> {
    std::mem::take(&mut self.published_pids)
  }
}

/// Unix Discord IPC transport.
pub struct IpcTransport {
  token: CancellationToken,
  accept_task: Option<JoinHandle<()>>,
  conns: Arc<tokio::sync::Mutex<JoinSet<()>>>,
  /// Live connection sockets (one clone per pump): shutdown closes them
  /// so pumps parked in blocking reads exit instead of outliving the
  /// drain deadline (`abort_all` cannot interrupt a running closure).
  live: Arc<Mutex<Vec<(u64, std::os::unix::net::UnixStream)>>>,
  bound_path: String,
  dirs: Vec<PathBuf>,
  sink: EventSink,
}

impl std::fmt::Debug for IpcTransport {
  /// Bound path only; live sockets stay out of logs.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("IpcTransport")
      .field("bound_path", &self.bound_path)
      .finish_non_exhaustive()
  }
}

impl IpcTransport {
  /// Bind `discord-ipc-0..=9`, preferring the official Flatpak Discord dir
  /// and falling back to the conventional candidate bases.
  ///
  /// # Errors
  ///
  /// [`AdsumError::IpcBind`] when every index is held by a live holder.
  pub async fn bind(user: Arc<Mutex<RpcUser>>) -> Result<(Self, mpsc::Receiver<ActivityCmd>)> {
    let (preferred, bind_dirs, aliases) = tokio::task::spawn_blocking(|| {
      let bases = socket_dir_candidates();
      let preferred = prepare_preferred_dir();
      let aliases = socket_alias_dirs(&bases);
      let bind_dirs = bind_dirs_in_order(bases, preferred.as_deref());
      (preferred, bind_dirs, aliases)
    })
    .await
    .map_err(|_| AdsumError::IpcBind {
      attempts: 10,
      source: std::io::Error::other("socket path preparation panicked"),
    })?;
    let (transport, rx) = Self::bind_with_paths(user, bind_dirs, aliases).await?;
    if let Some(dir) = &preferred
      && !Path::new(transport.socket_path()).starts_with(dir)
    {
      tracing::warn!(
        "[ipc] Preferred Flatpak socket dir {} not used; Flatpak Discord compatibility may be unavailable",
        dir.display()
      );
    }
    Ok((transport, rx))
  }

  /// Bind within explicit `dirs`, trying each in order (first = primary;
  /// tests point here at scratch dirs instead of the real runtime dirs).
  /// Fan-out links cover every existing alias layout under those dirs.
  ///
  /// # Errors
  ///
  /// [`AdsumError::IpcBind`] when no candidate dir binds (every index held
  /// by a live holder, or every dir unusable).
  pub async fn bind_with_dirs(
    user: Arc<Mutex<RpcUser>>,
    dirs: Vec<PathBuf>,
  ) -> Result<(Self, mpsc::Receiver<ActivityCmd>)> {
    let bind_dirs = if dirs.is_empty() {
      vec![PathBuf::from("/tmp")]
    } else {
      dirs
    };
    let aliases = tokio::task::spawn_blocking({
      let bind_dirs = bind_dirs.clone();
      move || socket_alias_dirs(&bind_dirs)
    })
    .await
    .map_err(|_| AdsumError::IpcBind {
      attempts: 10,
      source: std::io::Error::other("socket alias discovery panicked"),
    })?;
    Self::bind_with_paths(user, bind_dirs, aliases).await
  }

  /// Bind in `bind_dirs` (first = primary) and fan out into `alias_dirs`;
  /// keeping the lists separate stops production binding in alias dirs.
  async fn bind_with_paths(
    user: Arc<Mutex<RpcUser>>,
    bind_dirs: Vec<PathBuf>,
    alias_dirs: Vec<PathBuf>,
  ) -> Result<(Self, mpsc::Receiver<ActivityCmd>)> {
    // Blocking syscalls (bind, probe with its 1s budget, symlink fan-out)
    // run off the async workers; this executes once per (re)bind.
    let aliases_for_bind = alias_dirs.clone();
    let (std_listener, bound_path) = tokio::task::spawn_blocking(move || {
      let mut last_err = None;
      for (index, dir) in bind_dirs.iter().enumerate() {
        let base = format!("{}/discord-ipc", dir.display());
        match create_socket(&base, &aliases_for_bind) {
          Ok(bound) => return Ok(bound),
          Err(err) => {
            tracing::warn!("[ipc] Candidate dir {index} unusable, trying next: {err}");
            last_err = Some(err);
          }
        }
      }
      Err(last_err.unwrap_or(AdsumError::IpcBind {
        attempts: 10,
        source: std::io::Error::other("no candidate dirs"),
      }))
    })
    .await
    .map_err(|_| AdsumError::IpcBind {
      attempts: 10,
      source: std::io::Error::other("bind task panicked"),
    })??;
    std_listener
      .set_nonblocking(true)
      .map_err(|source| AdsumError::IpcBind {
        attempts: 10,
        source,
      })?;
    let listener = UnixListener::from_std(std_listener).map_err(|source| AdsumError::IpcBind {
      attempts: 10,
      source,
    })?;

    let (sink, rx) = EventSink::bounded(DEFAULT_IPC_QUEUE);
    let token = CancellationToken::new();
    let conns = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
    let live = Arc::new(Mutex::new(Vec::new()));
    let next_conn_id = Arc::new(AtomicU64::new(1));
    let accept_task = tokio::spawn(accept_loop(AcceptCtx {
      listener,
      token: token.clone(),
      conns: Arc::clone(&conns),
      pump_sem: Arc::new(tokio::sync::Semaphore::new(MAX_IPC_PUMPS)),
      live: Arc::clone(&live),
      next_conn_id: Arc::clone(&next_conn_id),
      user,
      sink: sink.clone(),
    }));

    Ok((
      Self {
        token,
        accept_task: Some(accept_task),
        conns,
        live,
        bound_path,
        dirs: alias_dirs,
        sink,
      },
      rx,
    ))
  }

  /// Filesystem path of the bound socket (for snapshots/diagnostics).
  #[must_use]
  pub fn socket_path(&self) -> &str {
    &self.bound_path
  }

  /// Commands shed by a full/closed sink since bind.
  #[must_use]
  pub fn dropped_total(&self) -> u64 {
    self.sink.dropped_total()
  }

  /// Shared event sink for census queue-depth sampling.
  #[must_use]
  pub fn event_sink(&self) -> EventSink {
    self.sink.clone()
  }

  /// Graceful shutdown: stop accepting, unblock connection pumps,
  /// drain connections with a deadline, then remove the socket file and
  /// fan-out links (via [`Drop`]).
  pub async fn shutdown(mut self) {
    self.token.cancel();
    if let Some(task) = self.accept_task.take() {
      let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
    // Close every live socket first: pumps parked in blocking reads exit
    // at once instead of holding their tasks past the drain deadline
    // (`abort_all` cannot interrupt a running closure).
    for (_, stream) in self
      .live
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .drain(..)
    {
      let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let mut owned = {
      let mut guard = self.conns.lock().await;
      std::mem::take(&mut *guard)
    };
    let deadline = std::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
    while !owned.is_empty() {
      let remaining = deadline.saturating_duration_since(std::time::Instant::now());
      if remaining.is_zero() {
        break;
      }
      match tokio::time::timeout(remaining, owned.join_next()).await {
        Ok(_) => {}
        Err(_) => break,
      }
    }
    owned.abort_all();
  }
}

/// Put the prepared Flatpak dir before the conventional bases.
fn bind_dirs_in_order(bases: Vec<PathBuf>, preferred: Option<&Path>) -> Vec<PathBuf> {
  let mut bind_dirs = Vec::with_capacity(bases.len() + 1);
  if let Some(dir) = preferred {
    bind_dirs.push(dir.to_path_buf());
  }
  bind_dirs.extend(bases);
  bind_dirs
}

/// Prepare the official Flatpak Discord dir, warning and falling back when
/// the runtime base is missing, shared-temp, or symlinked.
fn prepare_preferred_dir() -> Option<PathBuf> {
  let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") else {
    tracing::debug!("[ipc] XDG_RUNTIME_DIR is unset; skipping Flatpak socket dir");
    return None;
  };
  let Some(dir) = xdg
    .to_str()
    .and_then(|xdg| preferred_socket_dir_from(Some(xdg)))
  else {
    tracing::warn!(
      "[ipc] XDG_RUNTIME_DIR is not usable for the Flatpak Discord socket dir; Flatpak compatibility may be unavailable"
    );
    return None;
  };
  match prepare_socket_dir(&dir) {
    Ok(()) => Some(dir),
    Err(err) => {
      tracing::warn!(
        "[ipc] Cannot use Flatpak Discord socket dir {}: {err}; falling back",
        dir.display()
      );
      None
    }
  }
}

impl Drop for IpcTransport {
  /// Best-effort shutdown without awaiting: cancel first so token-aware
  /// loops observe it, close every live socket (unblocking pumps parked in
  /// blocking reads, which then emit their disconnect clears), abort the
  /// accept task, and clean our socket files. Connection tasks are *not*
  /// aborted here — closing their sockets ends them on their own, with
  /// clears intact. Best-effort filesystem cleanup (mirrors the legacy
  /// Drop): never touch foreign files (same shape check inside).
  fn drop(&mut self) {
    self.token.cancel();
    if let Some(task) = &self.accept_task {
      task.abort();
    }
    for (_, stream) in self
      .live
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .drain(..)
    {
      let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    tracing::info!("[ipc] Cleaning up socket: {}", self.bound_path);
    remove_socket_links(&self.dirs, &self.bound_path);
    let _ = std::fs::remove_file(&self.bound_path);
  }
}

/// Bind `base-0..=9` with stale reclaim, then fan out links into
/// `alias_dirs`.
fn create_socket(
  base: &str,
  alias_dirs: &[PathBuf],
) -> Result<(std::os::unix::net::UnixListener, String)> {
  let mut last_err = None;
  for tries in 0..=9_u8 {
    let socket_path = format!("{base}-{tries}");
    tracing::info!("[ipc] Creating socket: {socket_path}");
    match std::os::unix::net::UnixListener::bind(&socket_path) {
      Ok(socket) => {
        tracing::info!("[ipc] Created IPC socket: {socket_path}");
        fanout_socket_link(alias_dirs, &socket_path, &socket_file_name(&socket_path));
        return Ok((socket, socket_path));
      }
      Err(err) => {
        if err.kind() == ErrorKind::AddrInUse {
          tracing::info!("[ipc] Socket {socket_path} already in use, checking if stale...");
          if socket_holder_alive(&socket_path) {
            tracing::warn!("[ipc] Socket {socket_path} is in use by another process");
          } else {
            tracing::warn!("[ipc] Socket {socket_path} is stale, removing and retrying...");
            let _ = std::fs::remove_file(&socket_path);
            match std::os::unix::net::UnixListener::bind(&socket_path) {
              Ok(socket) => {
                tracing::info!("[ipc] Created IPC socket after cleaning stale: {socket_path}");
                fanout_socket_link(alias_dirs, &socket_path, &socket_file_name(&socket_path));
                return Ok((socket, socket_path));
              }
              Err(retry_err) => {
                tracing::warn!("[ipc] Rebind after stale-clean failed: {retry_err}");
              }
            }
          }
        } else {
          tracing::warn!("[ipc] Failed to create IPC socket, trying next: {err}");
        }
        last_err = Some(err);
      }
    }
  }
  Err(AdsumError::IpcBind {
    attempts: 10,
    source: last_err.unwrap_or_else(|| std::io::Error::other("no socket bound")),
  })
}

/// Cap on concurrent Unix IPC pumps (Windows parity in kind, lower in
/// value): each parks a `spawn_blocking` thread in a blocking pump, and
/// the CLI runtime allows 32 blocking threads total — past 16, peers are
/// dropped at once instead of starving the pool (or the process) under
/// connection floods, leaving room for unrelated blocking work (scanner
/// builds, snapshot writes).
const MAX_IPC_PUMPS: usize = 16;

/// Accept-loop state (moved into the accept task).
struct AcceptCtx {
  listener: UnixListener,
  token: CancellationToken,
  conns: Arc<tokio::sync::Mutex<JoinSet<()>>>,
  /// Bounds concurrent pumps (see `MAX_IPC_PUMPS`).
  pump_sem: Arc<tokio::sync::Semaphore>,
  live: Arc<Mutex<Vec<(u64, std::os::unix::net::UnixStream)>>>,
  next_conn_id: Arc<AtomicU64>,
  user: Arc<Mutex<RpcUser>>,
  sink: EventSink,
}

/// Accept loop: spawn one pump per peer until token cancel (shutdown).
async fn accept_loop(
  AcceptCtx {
    listener,
    token,
    conns,
    pump_sem,
    live,
    next_conn_id,
    user,
    sink,
  }: AcceptCtx,
) {
  loop {
    tokio::select! {
      biased;
      () = token.cancelled() => break,
      accepted = listener.accept() => {
        let (tok_stream, _) = match accepted {
          Ok(pair) => pair,
          Err(err) => {
            // Persistent readiness errors (fd exhaustion) would otherwise
            // hot-spin this loop: back off briefly, like the Windows
            // WouldBlock poll below. Transient per-connection failures
            // retry at once. Cancelled tokens still break promptly at the
            // top of the next iteration.
            tracing::warn!("[ipc] Accept failed: {err}");
            if let Some(backoff) = accept_error_backoff(&err) {
              tokio::time::sleep(backoff).await;
            }
            continue;
          }
        };
        let std_stream = match tok_stream.into_std() {
          Ok(stream) => stream,
          Err(err) => {
            tracing::warn!("[ipc] Failed to hand stream to blocking pump: {err}");
            continue;
          }
        };
        // tokio sockets are non-blocking; the pump does blocking I/O.
        if let Err(err) = std_stream.set_nonblocking(false) {
          tracing::warn!("[ipc] Failed to restore blocking mode: {err}");
          continue;
        };
        tracing::debug!("[ipc] Incoming stream...");
        let facil = ConnFacilitator::fresh(user.clone(), sink.clone());
        // Bounded pumps (Windows parity): past the cap the peer is
        // dropped at once instead of parking another blocking thread.
        // The permit rides into the closure and releases when the pump
        // returns.
        let Ok(pump_permit) = pump_sem.clone().try_acquire_owned() else {
          tracing::warn!("[ipc] Pump cap reached, dropping peer");
          continue;
        };
        // Track one clone per pump so shutdown can close it (unblocking
        // the pump's read); the pump unregisters itself on exit, bounding
        // the list during normal operation.
        let conn_id = next_conn_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(probe) = std_stream.try_clone() {
          live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((conn_id, probe));
        }
        let live_exit = Arc::clone(&live);
        // Short critical section: spawn_blocking is synchronous. Exited
        // tasks are reaped here so connection churn cannot pin JoinSet
        // entries for the transport lifetime.
        let mut conns = conns.lock().await;
        while conns.try_join_next().is_some() {}
        conns.spawn_blocking(move || {
          let _permit = pump_permit;
          let mut facil = facil;
          let mut stream = std_stream;
          handle_stream(&mut facil, &mut stream);
          live_exit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _)| *id != conn_id);
        });
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The prepared Flatpak dir is tried before every conventional base.
  #[test]
  fn bind_dirs_put_preferred_first() {
    let bases = vec![PathBuf::from("/run/user/1000"), PathBuf::from("/tmp")];
    let preferred = PathBuf::from("/run/user/1000/app/com.discordapp.Discord");
    let dirs = bind_dirs_in_order(bases, Some(&preferred));
    assert_eq!(
      dirs,
      vec![
        PathBuf::from("/run/user/1000/app/com.discordapp.Discord"),
        PathBuf::from("/run/user/1000"),
        PathBuf::from("/tmp"),
      ]
    );
  }

  /// Without a prepared Flatpak dir, the conventional order is unchanged.
  #[test]
  fn bind_dirs_without_preferred_keep_bases() {
    let bases = vec![PathBuf::from("/run/user/1000"), PathBuf::from("/tmp")];
    assert_eq!(bind_dirs_in_order(bases.clone(), None), bases);
  }

  /// Transient per-connection failures retry at once; persistent ones
  /// pause instead of hot-spinning the accept loop.
  #[test]
  fn accept_errors_back_off_only_when_persistent() {
    assert_eq!(
      accept_error_backoff(&std::io::Error::from(ErrorKind::ConnectionAborted)),
      None
    );
    assert_eq!(
      accept_error_backoff(&std::io::Error::from(ErrorKind::Interrupted)),
      None
    );
    assert_eq!(
      accept_error_backoff(&std::io::Error::from(ErrorKind::PermissionDenied)),
      Some(Duration::from_millis(50))
    );
  }
}
