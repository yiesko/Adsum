//! Unix IPC server: async accept loop, blocking pumps in a [`JoinSet`].
//!
//! The listener isTokio-native (no 50ms poll sleep); each connection runs
//! `handle_stream` in `spawn_blocking` (blocking socket I/O must never sit
//! on an async worker). Shutdown cancels the accept loop, drains the set
//! with a deadline, then aborts stragglers.

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::io;
use std::io::ErrorKind;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
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
use crate::markers::{
  OwnedAlias, PublishOutcome, publish_alias, recover_orphan_aliases, remove_owned_aliases,
};
use crate::paths::{
  bind_dirs_in_order, current_euid, preferred_socket_dir_from, prepare_fallback_dir,
  prepare_socket_dir, socket_alias_dirs, socket_dir_candidates, validate_private_dir,
};
use crate::probe::{
  FileId, SocketState, probe_socket, unlink_socket_if_unchanged, unlink_stale_socket_if_unchanged,
};
use crate::sink::{DEFAULT_IPC_QUEUE, EventSink};

/// Upper bound for graceful connection drain in [`IpcTransport::shutdown`].
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Exact warning when the socket is not inside the Flatpak-preferred dir.
const FLATPAK_UNAVAILABLE_WARNING: &str = "[ipc] Flatpak-compatible socket path unavailable; compatibility aliases may not be reachable from sandboxed clients";

/// Whether the bound socket sits inside the prepared Flatpak-preferred dir.
fn flatpak_compatibility_available(preferred: Option<&Path>, bound: &Path) -> bool {
  preferred.is_some_and(|dir| bound.starts_with(dir))
}

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
  /// `(dev, ino)` of the bound socket, rechecked before removal.
  bound_id: FileId,
  /// Aliases with valid markers created by this transport, removed on drop.
  owned_aliases: Vec<OwnedAlias>,
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
      let fallback = prepare_fallback_dir(
        &[PathBuf::from("/tmp"), PathBuf::from("/var/tmp")],
        current_euid(),
      );
      let aliases = socket_alias_dirs(&bases);
      let bind_dirs = bind_dirs_in_order(bases, preferred.as_deref(), fallback);
      (preferred, bind_dirs, aliases)
    })
    .await
    .map_err(|_| AdsumError::IpcBind {
      attempts: 10,
      source: std::io::Error::other("socket path preparation panicked"),
    })?;
    let (transport, rx) = Self::bind_with_paths(user, bind_dirs, aliases).await?;
    let bound = Path::new(transport.socket_path());
    if !flatpak_compatibility_available(preferred.as_deref(), bound) {
      tracing::warn!("{FLATPAK_UNAVAILABLE_WARNING}");
    }
    Ok((transport, rx))
  }

  /// Bind within explicit `dirs`, trying each in order (first = primary;
  /// tests point here at scratch dirs instead of the real runtime dirs).
  /// Marked aliases are published into every existing alias layout.
  ///
  /// # Errors
  ///
  /// [`AdsumError::IpcBind`] when `dirs` is empty, or when no candidate dir
  /// binds (every index held by a live holder, or every dir unusable).
  pub async fn bind_with_dirs(
    user: Arc<Mutex<RpcUser>>,
    dirs: Vec<PathBuf>,
  ) -> Result<(Self, mpsc::Receiver<ActivityCmd>)> {
    if dirs.is_empty() {
      return Err(AdsumError::IpcBind {
        attempts: 10,
        source: std::io::Error::new(
          std::io::ErrorKind::InvalidInput,
          "no bind directories were provided",
        ),
      });
    }
    let aliases = tokio::task::spawn_blocking({
      let dirs = dirs.clone();
      move || socket_alias_dirs(&dirs)
    })
    .await
    .map_err(|_| AdsumError::IpcBind {
      attempts: 10,
      source: std::io::Error::other("socket alias discovery panicked"),
    })?;
    Self::bind_with_paths(user, dirs, aliases).await
  }

  /// Bind in `bind_dirs` (first = primary) and publish marked aliases into
  /// `alias_dirs`; keeping the lists separate stops production binding in
  /// alias dirs.
  async fn bind_with_paths(
    user: Arc<Mutex<RpcUser>>,
    bind_dirs: Vec<PathBuf>,
    alias_dirs: Vec<PathBuf>,
  ) -> Result<(Self, mpsc::Receiver<ActivityCmd>)> {
    // Blocking syscalls (recovery, bind, probe with its 1s budget, alias
    // publication) run off the async workers; this executes once per bind.
    let (std_listener, bound_path, bound_id, mut owned_aliases) =
      tokio::task::spawn_blocking(move || {
        let euid = current_euid();
        // Recover dead leftovers before binding: a freshly bound socket must
        // never be mistaken for the target recorded in an old marker.
        recover_orphan_aliases(&alias_dirs, euid);
        let mut last_err = None;
        for (index, dir) in bind_dirs.iter().enumerate() {
          match create_socket(dir) {
            Ok((listener, bound_path, bound_id)) => {
              let owned_aliases = publish_aliases(&alias_dirs, &bound_path, bound_id, euid);
              return Ok((listener, bound_path, bound_id, owned_aliases));
            }
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
    // The listener stays alive until its filesystem entries are cleaned so
    // the identity recheck cannot race a replacement by dropping our fd.
    if let Err(source) = std_listener.set_nonblocking(true) {
      cleanup_failed_bind(&mut owned_aliases, &bound_path, bound_id);
      return Err(AdsumError::IpcBind {
        attempts: 10,
        source,
      });
    }
    let listener = match UnixListener::from_std(std_listener) {
      Ok(listener) => listener,
      Err(source) => {
        cleanup_failed_bind(&mut owned_aliases, &bound_path, bound_id);
        return Err(AdsumError::IpcBind {
          attempts: 10,
          source,
        });
      }
    };

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
        bound_id,
        owned_aliases,
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
  /// owned aliases (via [`Drop`]).
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
  /// accept task, then remove our marked aliases and the bound socket.
  /// Every removal revalidates the recorded identity, so foreign or
  /// replaced entries are never touched. Connection tasks are *not*
  /// aborted here — closing their sockets ends them on their own, with
  /// clears intact.
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
    remove_owned_aliases(&mut self.owned_aliases, current_euid());
    if !unlink_socket_if_unchanged(Path::new(&self.bound_path), self.bound_id) {
      tracing::debug!("[ipc] Bound socket {} changed; leaving it", self.bound_path);
    }
  }
}

/// Remove the aliases and socket of a bind that could not be handed to Tokio.
fn cleanup_failed_bind(owned: &mut Vec<OwnedAlias>, bound_path: &str, bound_id: FileId) {
  remove_owned_aliases(owned, current_euid());
  if !unlink_socket_if_unchanged(Path::new(bound_path), bound_id) {
    tracing::debug!("[ipc] Bound socket {bound_path} changed; leaving it");
  }
}

/// Publish a marked alias for every alias dir, never for the bound path.
fn publish_aliases(
  alias_dirs: &[PathBuf],
  bound_path: &str,
  bound_id: FileId,
  euid: u32,
) -> Vec<OwnedAlias> {
  let bound = Path::new(bound_path);
  let Some(index) = alias_index(bound) else {
    tracing::warn!("[ipc] Cannot derive a socket index from {bound_path}; skipping aliases");
    return Vec::new();
  };
  let mut owned = Vec::with_capacity(alias_dirs.len());
  for dir in alias_dirs {
    let alias = dir.join(format!("discord-ipc-{index}"));
    if alias.as_path() == bound {
      continue;
    }
    match publish_alias(dir, index, bound, bound_id, euid) {
      PublishOutcome::Created(created) => owned.push(created),
      PublishOutcome::Skipped => {}
    }
  }
  owned
}

/// `discord-ipc-<index>` index encoded in a bound socket path.
fn alias_index(bound: &Path) -> Option<u8> {
  bound
    .file_name()?
    .to_str()?
    .strip_prefix("discord-ipc-")?
    .parse::<u8>()
    .ok()
}

/// Bind `discord-ipc-0..=9` in the private `dir`, probing every index
/// before touching it; aliases are published by the caller afterwards.
fn create_socket(dir: &Path) -> Result<(std::os::unix::net::UnixListener, String, FileId)> {
  validate_private_dir(dir).map_err(|source| AdsumError::IpcBind {
    attempts: 10,
    source,
  })?;
  let mut last_err = None;
  for index in 0..=9_u8 {
    let path = dir.join(format!("discord-ipc-{index}"));
    let probe = probe_socket(&path);
    match probe.state {
      SocketState::Missing => {}
      SocketState::Stale => {
        let Some(identity) = probe.identity else {
          continue;
        };
        // The fresh probe narrows, but does not close, the TOCTOU window
        // before unlink; the structural protection is the validated private
        // directory.
        if !unlink_stale_socket_if_unchanged(&path, identity) {
          tracing::warn!(
            "[ipc] Stale socket {} changed or is no longer a reclaimable stale socket; leaving it",
            path.display()
          );
          continue;
        }
        tracing::warn!("[ipc] Reclaiming stale socket: {}", path.display());
      }
      SocketState::Active | SocketState::Unknown | SocketState::NotSocket => {
        tracing::info!(
          "[ipc] Socket {} is {:?}; trying next index",
          path.display(),
          probe.state
        );
        continue;
      }
    }
    let (listener, temp) = match bind_temp(dir, index) {
      Ok(pair) => pair,
      Err(err) => {
        last_err = Some(err);
        continue;
      }
    };
    if let Some(bound) = finish_bind(listener, &temp, &path) {
      return Ok(bound);
    }
    tracing::warn!(
      "[ipc] Could not secure socket {}; trying next index",
      path.display()
    );
  }
  Err(AdsumError::IpcBind {
    attempts: 10,
    source: last_err.unwrap_or_else(|| std::io::Error::other("no socket bound")),
  })
}

/// Bind a listener under a short private temp name in `dir`; the caller
/// installs it at the final path with a no-replace hard link.
fn bind_temp(
  dir: &Path,
  index: u8,
) -> std::io::Result<(std::os::unix::net::UnixListener, PathBuf)> {
  static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
  for _ in 0..8 {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Keep the temp name shorter than `discord-ipc-N`: a full `sun_path`
    // must not start failing only because publication uses a temp name.
    let temp = dir.join(format!(".b{index:x}{:x}{counter:x}", std::process::id()));
    tracing::info!("[ipc] Creating socket: {}", temp.display());
    match std::os::unix::net::UnixListener::bind(&temp) {
      Ok(listener) => return Ok((listener, temp)),
      Err(err) if err.kind() == ErrorKind::AddrInUse => continue,
      Err(err) => return Err(err),
    }
  }
  Err(std::io::Error::new(
    ErrorKind::AddrInUse,
    "temporary IPC socket name exhausted",
  ))
}

/// Linux install: open the temp socket first so a later name substitution
/// cannot redirect chmod or the final link.
#[cfg(target_os = "linux")]
fn finish_bind(
  listener: std::os::unix::net::UnixListener,
  temp: &Path,
  path: &Path,
) -> Option<(std::os::unix::net::UnixListener, String, FileId)> {
  let handle = open_socket_handle(temp)?;
  finish_bind_from_handle(listener, temp, path, handle)
}

/// Portable install: path-based fallback for non-Linux Unix platforms.
#[cfg(not(target_os = "linux"))]
fn finish_bind(
  listener: std::os::unix::net::UnixListener,
  temp: &Path,
  path: &Path,
) -> Option<(std::os::unix::net::UnixListener, String, FileId)> {
  // The directory protects this temp name from other UIDs, not from a
  // same-euid process. This identity is therefore whatever currently sits
  // there; the no-replace link protects `path`, not this temp-path race.
  let identity = socket_identity(temp)?;
  if std::fs::set_permissions(temp, std::fs::Permissions::from_mode(0o600)).is_err() {
    let _ = unlink_socket_if_unchanged(temp, identity);
    return None;
  }
  if socket_identity(temp) != Some(identity) {
    let _ = unlink_socket_if_unchanged(temp, identity);
    return None;
  }
  // `link` fails when anything already occupies `path`, so an entry that
  // appeared after the probe is preserved instead of adopted.
  if std::fs::hard_link(temp, path).is_err() {
    let _ = unlink_socket_if_unchanged(temp, identity);
    return None;
  }
  // Drop the temp name only after proving the final name carries our inode.
  let installed = socket_identity(path) == Some(identity);
  let _ = unlink_socket_if_unchanged(temp, identity);
  if !installed {
    tracing::warn!(
      "[ipc] Socket {} changed during installation; leaving it",
      path.display()
    );
    return None;
  }
  let socket_path = path.to_string_lossy().into_owned();
  tracing::info!("[ipc] Created IPC socket: {socket_path}");
  Some((listener, socket_path, identity))
}

/// Linux install: use the opened handle for chmod and the final link, then
/// remove the temp name only while it still names the same socket.
#[cfg(target_os = "linux")]
fn finish_bind_from_handle(
  listener: std::os::unix::net::UnixListener,
  temp: &Path,
  path: &Path,
  handle: OwnedFd,
) -> Option<(std::os::unix::net::UnixListener, String, FileId)> {
  let identity = socket_identity_from_fd(handle.as_fd())?;
  if chmod_socket_handle(handle.as_fd()).is_err() {
    let _ = unlink_socket_if_unchanged(temp, identity);
    return None;
  }
  if link_socket_handle(handle.as_fd(), path).is_err() {
    let _ = unlink_socket_if_unchanged(temp, identity);
    return None;
  }
  if socket_identity(path) != Some(identity) {
    let _ = unlink_socket_if_unchanged(path, identity);
    let _ = unlink_socket_if_unchanged(temp, identity);
    tracing::warn!(
      "[ipc] Socket {} changed during installation; leaving it",
      path.display()
    );
    return None;
  }
  let _ = unlink_socket_if_unchanged(temp, identity);
  let socket_path = path.to_string_lossy().into_owned();
  tracing::info!("[ipc] Created IPC socket: {socket_path}");
  Some((listener, socket_path, identity))
}

/// Apply `0600` through the handle, falling back to `/proc/self/fd`.
#[cfg(target_os = "linux")]
fn chmod_socket_handle(fd: BorrowedFd<'_>) -> io::Result<()> {
  // SAFETY: `fd` is valid and `c""` is a NUL-terminated empty path.
  if unsafe { libc::fchmodat(fd.as_raw_fd(), c"".as_ptr(), 0o600, libc::AT_EMPTY_PATH) } == 0 {
    return Ok(());
  }
  std::fs::set_permissions(
    proc_fd_path(fd.as_raw_fd()),
    std::fs::Permissions::from_mode(0o600),
  )
}

/// Hard-link the handle at `path` without replacing an existing entry.
#[cfg(target_os = "linux")]
fn link_socket_handle(fd: BorrowedFd<'_>, path: &Path) -> io::Result<()> {
  let target = CString::new(path.as_os_str().as_bytes())
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains nul"))?;
  // SAFETY: `fd` is valid, both paths are NUL-terminated, and the flags are
  // valid; the empty source asks `linkat` to link the descriptor itself.
  let linked = unsafe {
    libc::linkat(
      fd.as_raw_fd(),
      c"".as_ptr(),
      libc::AT_FDCWD,
      target.as_ptr(),
      libc::AT_EMPTY_PATH,
    )
  };
  if linked == 0 {
    return Ok(());
  }
  let proc_path = CString::new(proc_fd_path(fd.as_raw_fd()).as_os_str().as_bytes())
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "proc path contains nul"))?;
  // SAFETY: both paths are NUL-terminated and the fd is valid; following the
  // magic `/proc/self/fd` link targets the open socket.
  let linked = unsafe {
    libc::linkat(
      libc::AT_FDCWD,
      proc_path.as_ptr(),
      libc::AT_FDCWD,
      target.as_ptr(),
      libc::AT_SYMLINK_FOLLOW,
    )
  };
  if linked == 0 {
    Ok(())
  } else {
    Err(io::Error::last_os_error())
  }
}

/// `/proc/self/fd/<fd>`, the Linux fallback path for fd operations.
#[cfg(target_os = "linux")]
fn proc_fd_path(fd: RawFd) -> PathBuf {
  PathBuf::from(format!("/proc/self/fd/{fd}"))
}

/// Open the socket at `path` as an `O_PATH` handle without following links.
#[cfg(target_os = "linux")]
fn open_socket_handle(path: &Path) -> Option<OwnedFd> {
  let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
  // SAFETY: `c_path` is a valid NUL-terminated path and the flags are valid.
  let raw = unsafe {
    libc::open(
      c_path.as_ptr(),
      libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
  };
  if raw < 0 {
    return None;
  }
  // SAFETY: `raw` is a fresh descriptor owned by this function.
  let fd = unsafe { OwnedFd::from_raw_fd(raw) };
  socket_identity_from_fd(fd.as_fd()).map(|_| fd)
}

/// `(dev, ino)` of an open socket handle; `None` for any other file type.
#[cfg(target_os = "linux")]
fn socket_identity_from_fd(fd: BorrowedFd<'_>) -> Option<FileId> {
  let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
  // SAFETY: `fd` is valid and `stat` points to writable storage.
  if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
    return None;
  }
  // SAFETY: `fstat` succeeded and initialized `stat`.
  let stat = unsafe { stat.assume_init() };
  (stat.st_mode & libc::S_IFMT == libc::S_IFSOCK).then_some(FileId {
    dev: stat.st_dev,
    ino: stat.st_ino,
  })
}

/// Identity of `path` when it is a socket, without following symlinks.
fn socket_identity(path: &Path) -> Option<FileId> {
  let meta = std::fs::symlink_metadata(path).ok()?;
  meta.file_type().is_socket().then(|| FileId {
    dev: meta.dev(),
    ino: meta.ino(),
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
  use std::os::unix::fs::DirBuilderExt;
  use std::os::unix::net::{UnixListener, UnixStream};

  use super::*;

  /// Isolated 0700 temp dir per test, under canonical `/tmp` when available:
  /// macOS `$TMPDIR` is long and `sun_path` caps socket paths at 104 bytes.
  fn scratch(tag: &str) -> PathBuf {
    let base = std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(format!("adsum-unix-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
      .mode(0o700)
      .create(&dir)
      .expect("scratch dir");
    dir
  }

  /// A final path already occupied at install time is rejected and preserved.
  #[test]
  fn finish_bind_rejects_replaced_socket_path() {
    let dir = scratch("finish-bind-replaced");
    let path = dir.join("discord-ipc-0");
    let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
    let foreign = UnixListener::bind(&path).expect("foreign listener");
    let foreign_mode = std::fs::symlink_metadata(&path)
      .expect("foreign metadata")
      .mode()
      & 0o777;

    assert!(
      finish_bind(listener, &temp, &path).is_none(),
      "a path already naming another socket must be rejected"
    );
    assert!(
      std::fs::symlink_metadata(&path)
        .expect("foreign socket kept")
        .file_type()
        .is_socket(),
      "the foreign socket must stay at the path"
    );
    assert_eq!(
      std::fs::symlink_metadata(&path)
        .expect("foreign metadata")
        .mode()
        & 0o777,
      foreign_mode,
      "finish_bind must not chmod a foreign socket"
    );
    UnixStream::connect(&path).expect("foreign socket still accepts connections");
    assert!(
      std::fs::symlink_metadata(&temp).is_err(),
      "the private temp name must be cleaned up"
    );

    drop(foreign);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// A handle opened before the temp name is replaced must keep chmod/link
  /// on the original socket, never on the replacement.
  #[cfg(target_os = "linux")]
  #[test]
  fn finish_bind_from_handle_ignores_replaced_temp_name() {
    let dir = scratch("finish-bind-handle-race");
    let path = dir.join("discord-ipc-0");
    let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
    let handle = open_socket_handle(&temp).expect("socket handle");
    let original = socket_identity_from_fd(handle.as_fd()).expect("original identity");

    std::fs::remove_file(&temp).expect("remove temp name");
    let foreign = UnixListener::bind(&temp).expect("foreign listener");
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o666)).expect("foreign mode");
    let foreign_id = socket_identity(&temp).expect("foreign identity");
    assert_ne!(original, foreign_id, "the replacement must be distinct");

    let result = finish_bind_from_handle(listener, &temp, &path, handle);

    let foreign_mode = std::fs::symlink_metadata(&temp)
      .expect("foreign name must be preserved")
      .mode()
      & 0o777;
    assert_eq!(foreign_mode, 0o666, "foreign socket mode must not change");

    match result {
      Some((_, socket_path, identity)) => {
        assert_eq!(socket_path, path.to_string_lossy().as_ref());
        assert_eq!(identity, original);
        assert_eq!(
          socket_identity(&path),
          Some(original),
          "the final name must carry the original inode"
        );
      }
      None => {
        assert_ne!(
          socket_identity(&path),
          Some(foreign_id),
          "the foreign inode must never be installed"
        );
      }
    }

    drop(foreign);
    let _ = std::fs::remove_file(&temp);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// A renamed temp socket stays linkable: the fd install must succeed with
  /// the original inode and leave the foreign temp name untouched.
  #[cfg(target_os = "linux")]
  #[test]
  fn finish_bind_from_handle_installs_original_after_temp_rename() {
    let dir = scratch("finish-bind-handle-rename");
    let path = dir.join("discord-ipc-0");
    let renamed = dir.join("original-socket");
    let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
    let handle = open_socket_handle(&temp).expect("socket handle");
    let original = socket_identity_from_fd(handle.as_fd()).expect("original identity");

    std::fs::rename(&temp, &renamed).expect("rename temp name");
    let foreign = UnixListener::bind(&temp).expect("foreign listener");
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o666)).expect("foreign mode");
    let foreign_id = socket_identity(&temp).expect("foreign identity");
    assert_ne!(original, foreign_id, "the replacement must be distinct");

    let (_, socket_path, identity) =
      finish_bind_from_handle(listener, &temp, &path, handle).expect("fd install must succeed");

    assert_eq!(socket_path, path.to_string_lossy().as_ref());
    assert_eq!(
      identity, original,
      "the installed identity must be original"
    );
    assert_eq!(
      socket_identity(&path),
      Some(original),
      "the final name must carry the original inode"
    );
    assert_eq!(
      std::fs::symlink_metadata(&path)
        .expect("final metadata")
        .mode()
        & 0o777,
      0o600,
      "the original socket must be 0600"
    );
    assert_eq!(
      socket_identity(&temp),
      Some(foreign_id),
      "the foreign temp name must be preserved"
    );
    assert_eq!(
      std::fs::symlink_metadata(&temp)
        .expect("foreign metadata")
        .mode()
        & 0o777,
      0o666,
      "the foreign socket mode must not change"
    );

    drop(foreign);
    let _ = std::fs::remove_file(&temp);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&renamed);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// The temp name must be shorter than `discord-ipc-N`, preserving long paths.
  #[test]
  fn bind_temp_name_stays_shorter_than_final() {
    let dir = scratch("temp-name-length");
    let (listener, temp) = bind_temp(&dir, 0).expect("temp listener");
    let temp_name = temp.file_name().expect("temp name").to_string_lossy();
    assert!(
      temp_name.len() < "discord-ipc-0".len(),
      "temp name {temp_name} must leave room for a full sun_path"
    );

    drop(listener);
    let _ = std::fs::remove_file(&temp);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// The prepared Flatpak dir is tried before conventional bases, with
  /// shared temp roots filtered out.
  #[test]
  fn bind_dirs_put_preferred_first() {
    let bases = vec![PathBuf::from("/run/user/1000"), PathBuf::from("/tmp")];
    let preferred = PathBuf::from("/run/user/1000/app/com.discordapp.Discord");
    let dirs = bind_dirs_in_order(bases, Some(&preferred), None);
    assert_eq!(
      dirs,
      vec![
        PathBuf::from("/run/user/1000/app/com.discordapp.Discord"),
        PathBuf::from("/run/user/1000"),
      ]
    );
  }

  /// Without a prepared Flatpak dir, the conventional order is unchanged.
  #[test]
  fn bind_dirs_without_preferred_keep_bases() {
    let bases = vec![PathBuf::from("/run/user/1000")];
    assert_eq!(bind_dirs_in_order(bases.clone(), None, None), bases);
  }

  /// The Flatpak diagnostic is the exact operator-facing contract.
  #[test]
  fn flatpak_diagnostic_is_exact() {
    assert_eq!(
      FLATPAK_UNAVAILABLE_WARNING,
      "[ipc] Flatpak-compatible socket path unavailable; compatibility aliases may not be reachable from sandboxed clients"
    );
  }

  /// Compatibility holds only when the bound path is inside a prepared
  /// preferred dir; a string prefix is not a path prefix.
  #[test]
  fn flatpak_compatibility_detects_bound_path() {
    let preferred = Path::new("/run/user/1000/app/com.discordapp.Discord");
    assert!(flatpak_compatibility_available(
      Some(preferred),
      Path::new("/run/user/1000/app/com.discordapp.Discord/discord-ipc-0")
    ));
    assert!(!flatpak_compatibility_available(
      None,
      Path::new("/run/user/1000/app/com.discordapp.Discord/discord-ipc-0")
    ));
    assert!(!flatpak_compatibility_available(
      Some(preferred),
      Path::new("/run/user/1000/discord-ipc-0")
    ));
    assert!(!flatpak_compatibility_available(
      Some(preferred),
      Path::new("/run/user/1000/app/com.discordapp.DiscordCanary/discord-ipc-0")
    ));
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
