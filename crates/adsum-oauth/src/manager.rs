//! Shared RPC authentication policy and bounded, connection-owned OAuth work.

use std::io;
use std::num::NonZeroUsize;
use std::sync::{
  Arc, Mutex,
  atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
  mpsc::{self, Receiver, TryRecvError},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use adsum_types::{AppId, cmd::ActivityCmd, user::RpcUser};
use serde_json::Value;
use std::collections::HashMap;

use crate::{OAuthError, OAuthSession, RpcOAuthResult};

/// Authentication enforcement captured once when the service is constructed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RpcAuthPolicy {
  /// Preserve unauthenticated legacy Rich Presence.
  #[default]
  Permissive,
  /// Require a valid authorization for protected RPC commands.
  Required,
}

impl RpcAuthPolicy {
  #[must_use]
  pub fn from_env() -> Self {
    if crate::require_rpc_authentication() {
      Self::Required
    } else {
      Self::Permissive
    }
  }
}

/// Cooperative cancellation shared by an operation, its connection and daemon.
#[derive(Clone)]
pub struct OAuthCancellation {
  operation: Arc<AtomicBool>,
  connection: Arc<AtomicBool>,
  shutdown: Arc<AtomicBool>,
}

impl Default for OAuthCancellation {
  fn default() -> Self {
    Self {
      operation: Arc::new(AtomicBool::new(false)),
      connection: Arc::new(AtomicBool::new(false)),
      shutdown: Arc::new(AtomicBool::new(false)),
    }
  }
}

impl OAuthCancellation {
  #[must_use]
  pub fn is_cancelled(&self) -> bool {
    self.operation.load(Ordering::Acquire)
      || self.connection.load(Ordering::Acquire)
      || self.shutdown.load(Ordering::Acquire)
  }

  pub fn cancel(&self) {
    self.operation.store(true, Ordering::Release);
  }

  pub fn check(&self) -> Result<(), OAuthError> {
    if self.is_cancelled() {
      Err(OAuthError::Cancelled)
    } else {
      Ok(())
    }
  }
}

/// Backend boundary for hermetic tests and real Discord authorization.
///
/// Implementations must honor cancellation and bound every blocking I/O. They
/// must not log command arguments, authorization codes or access tokens.
pub trait OAuthBackend: Send + Sync {
  fn execute(
    &self,
    command: &ActivityCmd,
    expected_client_id: &str,
    cancellation: &OAuthCancellation,
  ) -> Result<RpcOAuthResult, OAuthError>;
}

/// Official browser flow and HTTPS token introspection; no client secret.
pub struct BrowserOAuthBackend {
  redirect_uri: Option<String>,
}

impl BrowserOAuthBackend {
  #[must_use]
  pub fn new(redirect_uri: Option<String>) -> Self {
    Self { redirect_uri }
  }
}

impl OAuthBackend for BrowserOAuthBackend {
  fn execute(
    &self,
    command: &ActivityCmd,
    expected_client_id: &str,
    cancellation: &OAuthCancellation,
  ) -> Result<RpcOAuthResult, OAuthError> {
    crate::handle_rpc_command_with_context(
      command,
      expected_client_id,
      self.redirect_uri.as_deref(),
      cancellation,
    )
  }
}

/// Snapshot of OAuth activity counters for telemetry and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OAuthMetrics {
  pub authorize_total: u64,
  pub authenticate_total: u64,
  pub cache_hits: u64,
  pub cache_misses: u64,
  pub rate_limited: u64,
}

/// Daemon-wide policy and work budget, shared by IPC and WebSocket.
///
/// Workers use a small, bounded set of dedicated threads rather than occupying
/// Tokio's blocking pool (which also serves IPC and scanner teardown).
pub struct OAuthManager {
  policy: RpcAuthPolicy,
  backend: Arc<dyn OAuthBackend>,
  limit: NonZeroUsize,
  active: AtomicUsize,
  shutdown: Arc<AtomicBool>,
  workers: Mutex<Vec<JoinHandle<()>>>,
  /// Minimum spacing between OAuth commands on one connection. Zero
  /// disables the limit (used by tests); `from_env` defaults to 250 ms.
  rate_limit: Duration,
  // Metrics
  pub authorize_total: AtomicU64,
  pub authenticate_total: AtomicU64,
  pub cache_hits: AtomicU64,
  pub cache_misses: AtomicU64,
  pub rate_limited: AtomicU64,
}

impl std::fmt::Debug for OAuthManager {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OAuthManager")
      .field("policy", &self.policy)
      .field("active", &self.active.load(Ordering::Acquire))
      .finish_non_exhaustive()
  }
}

impl OAuthManager {
  #[must_use]
  pub fn new(policy: RpcAuthPolicy, backend: Arc<dyn OAuthBackend>) -> Self {
    Self::with_capacity(
      policy,
      backend,
      NonZeroUsize::new(4).expect("nonzero constant"),
    )
  }

  #[must_use]
  pub fn with_capacity(
    policy: RpcAuthPolicy,
    backend: Arc<dyn OAuthBackend>,
    limit: NonZeroUsize,
  ) -> Self {
    Self {
      policy,
      backend,
      limit,
      active: AtomicUsize::new(0),
      shutdown: Arc::new(AtomicBool::new(false)),
      workers: Mutex::new(Vec::new()),
      rate_limit: Duration::ZERO,
      authorize_total: AtomicU64::new(0),
      authenticate_total: AtomicU64::new(0),
      cache_hits: AtomicU64::new(0),
      cache_misses: AtomicU64::new(0),
      rate_limited: AtomicU64::new(0),
    }
  }

  /// Same as [`Self::with_capacity`] but with an explicit rate-limit window:
  /// the deterministic form of [`Self::from_env`] for tests and embedders.
  #[must_use]
  pub fn with_rate_limit(
    policy: RpcAuthPolicy,
    backend: Arc<dyn OAuthBackend>,
    limit: NonZeroUsize,
    rate_limit: Duration,
  ) -> Self {
    Self {
      rate_limit,
      ..Self::with_capacity(policy, backend, limit)
    }
  }

  #[must_use]
  pub fn from_env() -> Self {
    // Production default: at most one OAuth command per 250 ms per
    // connection. Set `ADSUM_OAUTH_RATE_LIMIT_MS=0` to disable.
    let rate_limit = std::env::var("ADSUM_OAUTH_RATE_LIMIT_MS")
      .ok()
      .and_then(|v| v.parse::<u64>().ok())
      .map(Duration::from_millis)
      .unwrap_or(Duration::from_millis(250));
    Self {
      rate_limit,
      ..Self::new(
        RpcAuthPolicy::from_env(),
        Arc::new(BrowserOAuthBackend::new(
          std::env::var("ADSUM_OAUTH_REDIRECT_URI").ok(),
        )),
      )
    }
  }

  /// In-flight metrics snapshot, for telemetry and diagnostics.
  #[must_use]
  pub fn metrics(&self) -> OAuthMetrics {
    OAuthMetrics {
      authorize_total: self.authorize_total.load(Ordering::Relaxed),
      authenticate_total: self.authenticate_total.load(Ordering::Relaxed),
      cache_hits: self.cache_hits.load(Ordering::Relaxed),
      cache_misses: self.cache_misses.load(Ordering::Relaxed),
      rate_limited: self.rate_limited.load(Ordering::Relaxed),
    }
  }

  #[must_use]
  pub fn policy(&self) -> RpcAuthPolicy {
    self.policy
  }

  #[must_use]
  pub fn connection(self: &Arc<Self>, client_id: AppId) -> OAuthConnection {
    self.connection_with_restore(client_id, true)
  }

  /// Like [`Self::connection`] but with an explicit restore policy. Untrusted
  /// transports (the WebSocket server) pass `false`.
  #[must_use]
  pub fn connection_with_restore(
    self: &Arc<Self>,
    client_id: AppId,
    allow_restore: bool,
  ) -> OAuthConnection {
    OAuthConnection {
      manager: Arc::clone(self),
      client_id,
      closed: Arc::new(AtomicBool::new(false)),
      state: Arc::new(Mutex::new(ConnectionState::default())),
      allow_restore,
    }
  }

  /// Prevent new work and request cancellation without blocking the caller.
  pub fn cancel(&self) {
    self.shutdown.store(true, Ordering::Release);
  }

  /// Cancel and join workers. Call off async runtime threads. The production
  /// HTTPS request has a 12-second global deadline; callback waits cooperate.
  /// Each worker join is bounded so a wedged worker (e.g. stuck in HTTPS I/O)
  /// cannot hold the daemon past the systemd stop deadline. Returns the number
  /// of workers that did not exit in time.
  pub fn shutdown(&self) -> u32 {
    self.cancel();
    let workers = std::mem::take(&mut *self.workers.lock().unwrap_or_else(|e| e.into_inner()));
    let mut timed_out = 0u32;
    for worker in workers {
      match join_with_timeout(worker, Duration::from_secs(5)) {
        Ok(()) => {}
        Err(_) => {
          timed_out += 1;
          tracing::warn!("[daemon] OAuth worker did not exit within the shutdown deadline");
        }
      }
    }
    timed_out
  }
}

/// Join a thread with a deadline. The handle is moved to a waiter thread
/// so the caller can bound the wait instead of blocking indefinitely — a
/// wedged worker must not hold the shutdown past the systemd deadline.
fn join_with_timeout(handle: std::thread::JoinHandle<()>, timeout: Duration) -> io::Result<()> {
  let (tx, rx) = std::sync::mpsc::channel();
  let _waiter = std::thread::spawn(move || {
    let _ = handle.join();
    let _ = tx.send(());
  });
  rx.recv_timeout(timeout).map_err(|err| {
    io::Error::new(
      io::ErrorKind::TimedOut,
      format!("thread join timed out: {err}"),
    )
  })
}

const REST_CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Default)]
struct ConnectionState {
  session: Option<OAuthSession>,
  busy: bool,
  /// Short-lived cache of token-backed REST responses, keyed by path.
  /// Bounds memory: at most a few dozen entries per connection.
  cache: HashMap<String, (Arc<Value>, Instant)>,
  last_command: Option<Instant>,
}

/// Cloneable handle to exactly one connection and its immutable application ID.
/// Transports must call `close` on disconnect, pruning and shutdown.
#[derive(Clone)]
pub struct OAuthConnection {
  manager: Arc<OAuthManager>,
  client_id: AppId,
  closed: Arc<AtomicBool>,
  state: Arc<Mutex<ConnectionState>>,
  allow_restore: bool,
}

impl std::fmt::Debug for OAuthConnection {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OAuthConnection")
      .field("closed", &self.closed.load(Ordering::Acquire))
      .finish_non_exhaustive()
  }
}

impl OAuthConnection {
  #[must_use]
  pub fn client_id(&self) -> &AppId {
    &self.client_id
  }

  /// Forget identity immediately and make every late worker result unusable.
  pub fn close(&self) {
    self.closed.store(true, Ordering::Release);
    let lock = self.state.lock();
    let mut state = match lock {
      Ok(g) => g,
      Err(e) => e.into_inner(),
    };
    state.session = None;
    state.cache.clear();
  }

  /// Token-backed REST fetch with a short in-memory cache (30s) per
  /// connection. Avoids hammering Discord's API on repeated identical reads.
  /// Fresh cached REST response, if any. Lets a caller serve a hit without
  /// the blocking `cached_fetch` (the IPC pump uses it to stay responsive).
  #[must_use]
  pub fn cached_value(&self, path: &str) -> Option<Arc<Value>> {
    let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
    let (value, at) = state.cache.get(path)?;
    (at.elapsed() < REST_CACHE_TTL).then(|| Arc::clone(value))
  }

  pub fn cached_fetch(&self, path: &str) -> Result<Arc<Value>, OAuthError> {
    if self.closed.load(Ordering::Acquire) || self.manager.shutdown.load(Ordering::Acquire) {
      return Err(OAuthError::Cancelled);
    }
    let session = self.session()?;
    let Some(session) = session else {
      return Err(OAuthError::NotAuthenticated);
    };
    let token = session.access_token.clone();
    let now = Instant::now();
    {
      let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
      if let Some((value, at)) = state.cache.get(path)
        && now.duration_since(*at) < REST_CACHE_TTL
      {
        self.manager.cache_hits.fetch_add(1, Ordering::Relaxed);
        return Ok(Arc::clone(value));
      }
    }
    self.manager.cache_misses.fetch_add(1, Ordering::Relaxed);
    let value = Arc::new(crate::fetch_discord_json(&token, path)?);
    let mut state = self
      .state
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Do not cache a result whose session changed while the fetch was
    // in flight: the token may now belong to a different user.
    if self.closed.load(Ordering::Acquire)
      || state.session.as_ref().map(|s| s.access_token.as_str()) != Some(token.as_str())
    {
      return Ok(value);
    }
    // Bound the cache: evict oldest when over 64 entries.
    if state.cache.len() >= 64
      && let Some(oldest_key) = state
        .cache
        .iter()
        .min_by_key(|(_, (_, at))| *at)
        .map(|(k, _)| k.clone())
    {
      state.cache.remove(&oldest_key);
    }
    state
      .cache
      .insert(path.to_owned(), (Arc::clone(&value), now));
    Ok(value)
  }

  /// Live authentication and `scope` grant in one lock, without cloning the
  /// session: `Ok(None)` means unauthenticated, `Ok(Some(granted))` means a
  /// live session (expiry is cleared and reported like [`Self::session`]).
  pub fn auth_scope(&self, scope: &str) -> Result<Option<bool>, OAuthError> {
    self.with_live_session(|session| {
      session.map(|session| session.scopes.iter().any(|granted| granted == scope))
    })
  }

  /// Authenticated session presence and its user, if any, in one lock
  /// (expiry cleared and reported like [`Self::session`]) without cloning
  /// the rest of the session.
  pub fn session_user(&self) -> Result<(bool, Option<RpcUser>), OAuthError> {
    self.with_live_session(|session| match session {
      Some(session) => (true, session.user.clone()),
      None => (false, None),
    })
  }

  /// Return a live session, reporting expiry once instead of falling back to a
  /// cosmetic legacy identity for the request that discovered expiration.
  pub fn session(&self) -> Result<Option<OAuthSession>, OAuthError> {
    self.with_live_session(|session: Option<&OAuthSession>| session.cloned())
  }

  /// Run `f` against the live session under one lock, after the shared
  /// cancellation and expiry checks.
  fn with_live_session<T>(
    &self,
    f: impl FnOnce(Option<&OAuthSession>) -> T,
  ) -> Result<T, OAuthError> {
    if self.closed.load(Ordering::Acquire) || self.manager.shutdown.load(Ordering::Acquire) {
      return Err(OAuthError::Cancelled);
    }
    let lock = self.state.lock();
    let mut state = match lock {
      Ok(g) => g,
      Err(e) => e.into_inner(),
    };
    if state.session.as_ref().is_some_and(OAuthSession::is_expired) {
      state.session = None;
      return Err(OAuthError::InvalidAccessToken);
    }
    Ok(f(state.session.as_ref()))
  }

  /// Shared gate. `owned_clear` must be true only for an explicit null activity
  /// for a PID already tracked by this connection. Such cleanup remains legal
  /// after expiry or failed reauthentication, never for another client's card.
  pub fn check_command(&self, command: &ActivityCmd, owned_clear: bool) -> Result<(), OAuthError> {
    let oauth = matches!(command.cmd.as_ref(), "AUTHORIZE" | "AUTHENTICATE");
    let session = self.session();
    let authenticated = matches!(&session, Ok(Some(_)));
    if (oauth || authenticated || self.manager.policy == RpcAuthPolicy::Required)
      && command
        .application_id
        .as_ref()
        .is_some_and(|id| id != &self.client_id)
    {
      return Err(OAuthError::ApplicationMismatch);
    }
    let cleanup = owned_clear
      && command.cmd == "SET_ACTIVITY"
      && command
        .args
        .as_ref()
        .is_some_and(|args| matches!(args.activity, Some(None)));
    match session {
      Err(OAuthError::InvalidAccessToken) if cleanup || oauth => Ok(()),
      Err(error) => Err(error),
      Ok(_) if oauth || cleanup => Ok(()),
      Ok(None) if self.manager.policy == RpcAuthPolicy::Required => {
        Err(OAuthError::NotAuthenticated)
      }
      Ok(_) => Ok(()),
    }
  }

  /// Admit before spawning: at most one operation per connection and a bounded
  /// number daemon-wide. A rejected concurrent request cannot erase a session.
  pub fn start(&self, command: ActivityCmd) -> Result<OAuthTask, OAuthError> {
    if !matches!(command.cmd.as_ref(), "AUTHORIZE" | "AUTHENTICATE") {
      return Err(OAuthError::InvalidArguments);
    }
    crate::authorize::validate_client_id(&self.client_id)?;
    self.check_command(&command, false)?;

    // Untrusted transports (WebSocket) may not restore a session from the
    // daemon's stored grant or exchange a caller-supplied refresh token: both
    // paths mint a session without an access token.
    if !self.allow_restore && command.cmd == "AUTHENTICATE" {
      let has_access = command
        .args
        .as_ref()
        .and_then(|args| args.extra.get("access_token"))
        .and_then(Value::as_str)
        .is_some();
      if !has_access {
        return Err(OAuthError::InvalidAccessToken);
      }
    }

    // Rate limit: at most one OAuth command per second per connection.
    {
      let lock = self.state.lock();
      let state = match lock {
        Ok(g) => g,
        Err(e) => e.into_inner(),
      };
      let now = Instant::now();
      let window = self.manager.rate_limit;
      if !window.is_zero()
        && let Some(last) = state.last_command
        && now.duration_since(last) < window
      {
        self.manager.rate_limited.fetch_add(1, Ordering::Relaxed);
        return Err(OAuthError::RateLimited);
      }
    }

    let authenticate = command.cmd == "AUTHENTICATE";

    let lock = self.state.lock();
    let mut state = match lock {
      Ok(g) => g,
      Err(e) => e.into_inner(),
    };
    if state.busy {
      return Err(OAuthError::OperationInProgress);
    }
    let mut workers = self
      .manager
      .workers
      .lock()
      .unwrap_or_else(|e| e.into_inner());
    if self.closed.load(Ordering::Acquire) || self.manager.shutdown.load(Ordering::Acquire) {
      return Err(OAuthError::Cancelled);
    }
    let mut active = self.manager.active.load(Ordering::Acquire);
    loop {
      if active >= self.manager.limit.get() {
        return Err(OAuthError::WorkerUnavailable);
      }
      match self.manager.active.compare_exchange_weak(
        active,
        active + 1,
        Ordering::AcqRel,
        Ordering::Acquire,
      ) {
        Ok(_) => break,
        Err(current) => active = current,
      }
    }
    let permit = WorkerPermit(Arc::clone(&self.manager));
    state.busy = true;
    if authenticate {
      state.session = None;
      // Invalidate the REST cache on re-authentication: the new grant may
      // belong to a different user or scope set.
      state.cache.clear();
      self
        .manager
        .authenticate_total
        .fetch_add(1, Ordering::Relaxed);
    } else {
      self.manager.authorize_total.fetch_add(1, Ordering::Relaxed);
    }
    // Stamp the rate-limit window only after admission succeeds, so rejected
    // requests do not consume it.
    state.last_command = Some(Instant::now());
    drop(state);
    let cancellation = OAuthCancellation {
      operation: Arc::new(AtomicBool::new(false)),
      connection: Arc::clone(&self.closed),
      shutdown: Arc::clone(&self.manager.shutdown),
    };
    let worker_cancellation = cancellation.clone();
    let connection = self.clone();
    let (tx, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
      .name("adsum-oauth".into())
      .spawn(move || {
        let _permit = permit;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
          worker_cancellation.check()?;
          connection
            .manager
            .backend
            .execute(&command, &connection.client_id, &worker_cancellation)
        }))
        .unwrap_or(Err(OAuthError::WorkerUnavailable));
        let mut state = connection.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = if worker_cancellation.is_cancelled() {
          tracing::info!(
            application = %connection.client_id.as_ref(),
            "[oauth] {} cancelled before completing",
            command.cmd
          );
          Err(OAuthError::Cancelled)
        } else {
          result.and_then(|mut result| {
            if authenticate {
              let session = result
                .session
                .as_mut()
                .ok_or(OAuthError::UnexpectedDiscordResponse)?;
              if session.application_id != connection.client_id {
                return Err(OAuthError::ApplicationMismatch);
              }
              if session.is_expired() {
                return Err(OAuthError::InvalidAccessToken);
              }
              if !session.scopes.iter().any(|scope| scope == "rpc") {
                return Err(OAuthError::MissingRpcScope);
              }
              if !session.scopes.iter().any(|scope| scope == "identify") {
                session.user = None;
              }
              // Identity and scopes only: the bearer token never reaches a
              // log line (RpcUser has no credential fields).
              match &session.user {
                Some(user) => tracing::info!(
                  user_id = %user.id,
                  username = %user.username,
                  application = %session.application_id.as_ref(),
                  scopes = %session.scopes.join(" "),
                  expires = %session.expires.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                  "[oauth] authenticated"
                ),
                None => tracing::info!(
                  application = %session.application_id.as_ref(),
                  scopes = %session.scopes.join(" "),
                  expires = %session.expires.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                  "[oauth] authenticated (no identify scope; user withheld)"
                ),
              }
              state.session = Some(session.clone());
            } else {
              tracing::info!(
                application = %connection.client_id.as_ref(),
                "[oauth] authorization code issued"
              );
            }
            Ok(result)
          })
        };
        if let Err(error) = &result {
          // OAuthError is a plain enum of static strings: safe to log whole.
          tracing::warn!(
            application = %connection.client_id.as_ref(),
            command = %command.cmd,
            error = ?error,
            "[oauth] {} failed",
            command.cmd
          );
        }
        state.busy = false;
        drop(state);
        let _ = tx.send(result);
      });
    match worker {
      Ok(worker) => {
        let mut pending = Vec::with_capacity(workers.len() + 1);
        let mut finished = Vec::new();
        for old in workers.drain(..) {
          if old.is_finished() {
            finished.push(old);
          } else {
            pending.push(old);
          }
        }
        pending.push(worker);
        *workers = pending;
        drop(workers);
        // Thread-local destructors may still be running after is_finished().
        // Never join under the registry lock: destructors can reenter a backend.
        for old in finished {
          let _ = old.join();
        }
        Ok(OAuthTask {
          receiver,
          cancellation,
        })
      }
      Err(_) => {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).busy = false;
        Err(OAuthError::WorkerUnavailable)
      }
    }
  }
}

struct WorkerPermit(Arc<OAuthManager>);
impl Drop for WorkerPermit {
  fn drop(&mut self) {
    self.0.active.fetch_sub(1, Ordering::AcqRel);
  }
}

/// A single bounded reply mailbox. Dropping it cancels the operation; transports
/// can poll it without parking the IPC reader or Tokio's shared blocking pool.
pub struct OAuthTask {
  receiver: Receiver<Result<RpcOAuthResult, OAuthError>>,
  cancellation: OAuthCancellation,
}

impl std::fmt::Debug for OAuthTask {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OAuthTask").finish_non_exhaustive()
  }
}

impl OAuthTask {
  pub fn try_result(&self) -> Option<Result<RpcOAuthResult, OAuthError>> {
    match self.receiver.try_recv() {
      Ok(result) => Some(result),
      Err(TryRecvError::Empty) => None,
      Err(TryRecvError::Disconnected) => Some(Err(OAuthError::WorkerUnavailable)),
    }
  }

  /// Blocking wait for synchronous callers/tests, not async runtime threads.
  pub fn recv(&self) -> Result<RpcOAuthResult, OAuthError> {
    self
      .receiver
      .recv()
      .unwrap_or(Err(OAuthError::WorkerUnavailable))
  }

  pub fn cancel(&self) {
    self.cancellation.cancel();
  }
}

impl Drop for OAuthTask {
  fn drop(&mut self) {
    self.cancel();
  }
}
