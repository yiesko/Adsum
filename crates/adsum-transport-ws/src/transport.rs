//! Game WebSocket transport: bind loop, event pump, graceful shutdown.
//!
//! The P1 fix, structurally: the client map behind
//! `Arc<RwLock<HashMap<..>>>` is only ever touched in short clone / insert /
//! remove sections. Every `.await` (socket I/O, sink send, responder send)
//! happens with **no guard alive** — a concurrent `client_count()` snapshot
//! can never stall behind message flow (see
//! `snapshots_under_flood_never_stall`).

use std::{
  net::{Ipv4Addr, SocketAddr},
  sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
  },
  time::Duration,
};

use adsum_protocol::error::{AdsumError, Result};
use adsum_protocol::query::query_param;
use adsum_types::AppId;
use adsum_types::cmd::{ActivityCmd, ActivityCmdArgs};
use adsum_types::user::RpcUser;
use adsum_ws::{ClientId, CloseCode, Event, EventHub, Message, Responder};
use rustc_hash::FxHashMap;
use serde_json::Value;

use crate::handlers::PublishedSlot;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

use crate::config::WsTransportConfig;
use crate::handlers;

/// How long the pump waits for sink capacity before shedding (counted).
const SINK_SEND_TIMEOUT: Duration = Duration::from_millis(250);

/// Shared budget for one disconnect cleanup pass: clears retry until it
/// lapses instead of shedding on the first full queue, so a transiently
/// wedged bridge still gets its clears. Bounds the worst pump stall per
/// disconnect no matter how many pids were tracked.
const DISCONNECT_CLEAR_BUDGET: Duration = Duration::from_secs(1);

/// Cap on staged commands transport-wide: ~16 fully-wedged disconnects.
/// Past it the oldest entry sheds counted (visible via `dropped_total`).
/// Unbounded retention would let connection churn grow memory forever while
/// the bridge is wedged; every handoff/publish table in this workspace
/// sheds oldest the same way.
const MAX_PENDING_COMMANDS: usize = 256;

/// What a queued entry carries. Publishes and clears share one FIFO so a
/// clear can never overtake an older publish of the same pid — only a
/// publish needs the whole command, while a clear rebuilds its command at
/// emit time instead of retaining it.
#[derive(Debug, Clone)]
pub(crate) enum PendingPayload {
  /// A `SET_ACTIVITY` parked while the sink was full. Boxed: ~1 KB, and the
  /// queue can hold 256 of them.
  Publish(Box<ActivityCmd>),
  /// A disconnect clear: only the metadata a clear needs, so a stalled
  /// sink retains ~80 B per entry rather than a full command's ~1 KB.
  Clear(PendingClear),
}

impl PendingPayload {
  /// The command to hand downstream. A clear rebuilds its command here, so
  /// nothing larger than the slim metadata is ever retained (see
  /// [`PendingClear::into_cmd`]).
  fn into_cmd(self) -> ActivityCmd {
    match self {
      Self::Publish(cmd) => *cmd,
      Self::Clear(clear) => clear.into_cmd(),
    }
  }
}

/// Slim clear metadata: everything a disconnect clear carries, without
/// retaining the command it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingClear {
  /// Application the published card belonged to. Refcounted like the
  /// command's own id: this is cloned out per staged clear.
  pub(crate) app_id: Option<AppId>,
  /// Process whose card is being cleared (pid 0 is never genuine).
  pub(crate) pid: u64,
  /// Nonce of the publish this clear answers.
  pub(crate) nonce: Value,
}

impl PendingClear {
  /// Rebuild the clear command from the slim metadata. Byte-identical to
  /// [`crate::handlers::clear_for_slot`], which the invariant test pins.
  fn into_cmd(self) -> ActivityCmd {
    ActivityCmd {
      application_id: self.app_id,
      cmd: "SET_ACTIVITY".to_string(),
      data: None,
      evt: None,
      args: Some(ActivityCmdArgs {
        // pid 0 is never a genuine clear, so it is safely ignored
        // downstream — same convention as the whole-command clear.
        pid: Some(self.pid),
        activity: Some(None),
        code: None,
        user_id: None,
      }),
      nonce: self.nonce,
    }
  }

  /// Slim metadata for one published slot's clear.
  pub(crate) fn from_slot(published: &crate::handlers::PublishedSlot) -> Self {
    Self {
      app_id: published.app_id.clone(),
      pid: published.pid,
      nonce: published.nonce.clone(),
    }
  }

  /// Slim metadata for a clear the sink just refused.
  ///
  /// The inverse of [`into_cmd`](Self::into_cmd), which is what makes a
  /// refusal cheap to put back: the sink hands the command back untouched,
  /// so the entry returns as the metadata it was staged as instead of as
  /// the full command it was offered as — the difference between ~80 B and
  /// ~1 KB retained per entry for as long as the sink stays full.
  fn from_cmd(cmd: &ActivityCmd) -> Self {
    Self {
      app_id: cmd.application_id.clone(),
      pid: cmd
        .args
        .as_ref()
        .and_then(|args| args.pid)
        .unwrap_or_default(),
      nonce: cmd.nonce.clone(),
    }
  }
}

/// One staged queue entry: the payload plus its own expiry, so an older
/// retry task lapsing must never shed a newer entry (see `stage_publish`).
#[derive(Debug, Clone)]
pub(crate) struct PendingEntry {
  payload: PendingPayload,
  /// Lapses independently per entry (see `expire_pending`).
  deadline: std::time::Instant,
}

/// The staged queue: FIFO, so `pop_front`/`push_front` replace the
/// `remove(0)` memmove that used to run per delivered entry.
pub(crate) type PendingQueue = std::collections::VecDeque<PendingEntry>;

/// Discord origins allowed to drive game commands. Absent `origin`
/// (non-browser clients) passes: only a mismatched origin is refused.
const ALLOWED_ORIGINS: [&str; 3] = [
  "https://discord.com",
  "https://canary.discord.com",
  "https://ptb.discord.com",
];

/// Per-client slot: responder plus the publication history needed for
/// clear-on-disconnect.
///
/// No `Clone` derive (see `SlotSnapshot`): the history must only ever be
/// read under a short guard, never duplicated wholesale.
#[derive(Debug)]
struct ClientSlot {
  responder: Responder,
  /// Every pid published on this connection (slim records), oldest first.
  /// A single-pid client — the norm — keeps exactly one entry. Capped at
  /// [`handlers::MAX_TRACKED_PIDS`]: extras are refused before forwarding,
  /// so this history is always complete and disconnect cleanup clears
  /// every forwarded card.
  published: Vec<handlers::PublishedSlot>,
  /// `client_id` from the connect query, shared with the fallback the
  /// forwarded commands carry. Refcounted, so the per-message snapshot
  /// clone is a counter bump and never a heap copy.
  query_client_id: Option<AppId>,
}

/// Whether a publication was admitted to the bounded per-connection history.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Admission {
  /// Tracked (re-publishes refresh as most recent). Carries the displaced
  /// prior slot, if any, so a shed delivery can roll back to it instead of
  /// dropping tracking for a previously shown card.
  Admitted {
    /// Previous record for this pid (`None` for first-time publishes).
    displaced: Option<handlers::PublishedSlot>,
  },
  /// Unknown pid past the bound: refused before forwarding.
  RefusedFull,
}

impl ClientSlot {
  /// Record a publication, refreshing a re-published pid as most recent.
  /// Unknown pids past [`handlers::MAX_TRACKED_PIDS`] are refused: the
  /// caller must not forward them, keeping every forwarded pid tracked
  /// (and therefore covered by disconnect cleanup).
  fn note_published(&mut self, app_id: Option<AppId>, pid: u64, nonce: Value) -> Admission {
    if let Some(pos) = self.published.iter().position(|entry| entry.pid == pid) {
      let displaced = self.published.remove(pos);
      self
        .published
        .push(handlers::PublishedSlot { app_id, pid, nonce });
      Admission::Admitted {
        displaced: Some(displaced),
      }
    } else if self.published.len() >= handlers::MAX_TRACKED_PIDS {
      Admission::RefusedFull
    } else {
      self
        .published
        .push(handlers::PublishedSlot { app_id, pid, nonce });
      Admission::Admitted { displaced: None }
    }
  }

  /// Forget one tracked pid. Used when a genuine client clear lands
  /// (frees the slot for new pids). No-op when absent.
  fn forget_published(&mut self, pid: u64) {
    if let Some(pos) = self.published.iter().position(|entry| entry.pid == pid) {
      self.published.remove(pos);
      // Hand the buffer back once nothing is published: its capacity is
      // whatever a multiplexing client peaked at, and holding that for the
      // connection's whole life is how the per-connection footprint
      // ratchets.
      if self.published.is_empty() {
        self.published = Vec::new();
      }
    }
  }

  /// Roll back a shed admission: drop the undelivered record, restoring the
  /// displaced slot of a refresh so a previously shown card stays tracked
  /// (and clearable on disconnect). First-time publishes leave nothing.
  fn rollback_publish(&mut self, pid: u64, displaced: Option<handlers::PublishedSlot>) {
    self.forget_published(pid);
    if let Some(old) = displaced {
      self.published.push(old);
    }
  }
}

/// Outcome of a non-blocking sink enqueue: the refused command rides
/// along on `Full` so callers can stage it instead of dropping it.
#[derive(Debug)]
pub(crate) enum TryEmit {
  /// Accepted by the sink.
  Sent,
  /// Queue full; command returned (boxed: the 1KB body would dwarf the
  /// enum) for staging.
  Full(Box<ActivityCmd>),
  /// Receiver gone; no receiver will ever return.
  Closed,
}

/// Cheap per-message snapshot: `responder` is an `Arc` bump and the id is
/// a short string. Publication history is never cloned here — it is only
/// read on disconnect and appended on `SET_ACTIVITY`, both under short
/// write guards.
struct SlotSnapshot {
  responder: Responder,
  query_client_id: Option<AppId>,
}

impl SlotSnapshot {
  /// Cheap per-message snapshot of one stored slot.
  ///
  /// The client id is shared, never copied: this runs for **every**
  /// message — `SUBSCRIBE` and `GET_USER` included, which never use the
  /// id — so a deep copy here charged the whole message path. Publication
  /// history is deliberately left behind (read only on disconnect, under
  /// a short guard) and the read guard is dropped before any await.
  fn from_slot(slot: &ClientSlot) -> Self {
    Self {
      responder: slot.responder.clone(),
      query_client_id: slot.query_client_id.clone(),
    }
  }
}

/// Bounded event sink with a shed counter.
///
/// `send_timeout` waits briefly for genuine bursts, then drops (counted)
/// instead of stalling the pump — one slow bridge must not freeze every
/// game client. A closed sink (dead bridge) also counts and continues.
#[derive(Debug, Clone)]
pub(crate) struct Sink {
  tx: mpsc::Sender<ActivityCmd>,
  dropped: Arc<AtomicU64>,
}

impl Sink {
  /// Queue one command downstream, shedding (counted) instead of stalling
  /// the pump when the bridge stops draining. Returns whether the command
  /// was accepted: callers track a publication only on `true`, so a shed
  /// publish leaves no disconnect clear behind for a card that never showed.
  pub(crate) async fn emit(&self, cmd: ActivityCmd) -> bool {
    match self.tx.send_timeout(cmd, SINK_SEND_TIMEOUT).await {
      Ok(()) => true,
      Err(_) => {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("[transport-ws] Event sink full/closed, dropping command");
        false
      }
    }
  }

  /// Count `n` shed clears (shared by staging overflow and drains).
  fn count_shed(&self, n: u64) {
    self.dropped.fetch_add(n, Ordering::Relaxed);
  }

  /// Non-blocking enqueue for the staged-clear flush below: reports the
  /// outcome instead of waiting, so the pump never stalls on a wedge.
  /// A dedicated enum (not `Result`): returning the refused command back
  /// for re-staging would trip `result_large_err` on the 1KB command.
  pub(crate) fn try_emit(&self, cmd: ActivityCmd) -> TryEmit {
    match self.tx.try_send(cmd) {
      Ok(()) => TryEmit::Sent,
      Err(tokio::sync::mpsc::error::TrySendError::Full(cmd)) => TryEmit::Full(Box::new(cmd)),
      Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => TryEmit::Closed,
    }
  }

  /// Stage a parked publish for opportunistic flush, stamped with its own
  /// expiry. Bounded: past `MAX_PENDING_COMMANDS` the oldest sheds
  /// counted instead of growing memory without limit.
  pub(crate) fn stage_publish(&self, pending: &Mutex<PendingQueue>, cmd: ActivityCmd) {
    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    if pending.len() >= MAX_PENDING_COMMANDS {
      self.count_shed(1);
      tracing::warn!("[transport-ws] Pending commands full, shedding oldest");
      pending.pop_front();
    }
    pending.push_back(PendingEntry {
      payload: PendingPayload::Publish(Box::new(cmd)),
      deadline: std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET,
    });
  }

  /// Emit now, or stage for a later flush only when the sink is full.
  ///
  /// Trying first means a normal publish takes no queue lock at all. The
  /// previous path always staged and then immediately drained, so even a
  /// publish the sink could take right away paid two lock round-trips —
  /// and the entry left the queue in the very next statement.
  ///
  /// Order is preserved by construction: the caller flushed before
  /// staging, so either the queue is empty (nothing to overtake) or the
  /// sink is full and this goes to the back.
  pub(crate) fn emit_or_stage_publish(
    &self,
    pending: &Mutex<PendingQueue>,
    cmd: ActivityCmd,
  ) -> bool {
    match self.try_emit(cmd) {
      TryEmit::Sent => true,
      TryEmit::Full(cmd) => {
        self.stage_publish(pending, *cmd);
        true
      }
      TryEmit::Closed => {
        self.count_shed(1);
        tracing::warn!("[transport-ws] Event sink closed, dropping publish");
        false
      }
    }
  }

  /// Stage a disconnect clear for opportunistic flush, stamped with its own
  /// expiry. Staged slim: only the metadata a clear needs is retained (see
  /// [`PendingClear`]), so a wedged sink holds ~80 B instead of ~1 KB per
  /// entry.
  pub(crate) fn stage_clear(&self, pending: &Mutex<PendingQueue>, published: &PublishedSlot) {
    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    if pending.len() >= MAX_PENDING_COMMANDS {
      self.count_shed(1);
      tracing::warn!("[transport-ws] Pending commands full, shedding oldest");
      pending.pop_front();
    }
    pending.push_back(PendingEntry {
      payload: PendingPayload::Clear(PendingClear::from_slot(published)),
      deadline: std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET,
    });
  }

  /// Flush staged commands oldest-first, stopping at the first full queue
  /// (order preserved for the next flush). Delivery never expires: an
  /// unexpired entry always deserves its chance, whenever capacity
  /// returns. A closed sink drops the whole queue counted — no receiver
  /// will ever return. Never waits: safe on every pump path.
  pub(crate) fn flush_pending(&self, pending: &Mutex<PendingQueue>) {
    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(entry) = pending.pop_front() {
      // A refused clear goes back as the metadata it was staged as, not as
      // the command it was offered downstream: the command is ~1 KB and a
      // wedged sink would retain one per entry for as long as the queue
      // holds, which is the case the slim form exists for.
      let was_clear = matches!(entry.payload, PendingPayload::Clear(_));
      match self.try_emit(entry.payload.into_cmd()) {
        TryEmit::Sent => {}
        TryEmit::Full(cmd) => {
          // Put it back exactly where it was: order is a hard invariant
          // (a clear must never overtake an older publish of the same pid).
          pending.push_front(PendingEntry {
            payload: if was_clear {
              PendingPayload::Clear(PendingClear::from_cmd(&cmd))
            } else {
              PendingPayload::Publish(cmd)
            },
            deadline: entry.deadline,
          });
          break;
        }
        TryEmit::Closed => {
          self
            .dropped
            .fetch_add(pending.len() as u64, Ordering::Relaxed);
          tracing::warn!(
            "[transport-ws] Event sink closed, dropping {} staged command(s)",
            pending.len()
          );
          pending.clear();
          break;
        }
      }
    }
  }

  /// Shed only entries whose own deadline elapsed, counting each. An older
  /// retry task lapsing must never discard a newer entry that still owns
  /// a retry window.
  fn expire_pending(&self, pending: &Mutex<PendingQueue>) {
    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let mut expired = 0u64;
    pending.retain(|entry| {
      if now >= entry.deadline {
        expired += 1;
        false
      } else {
        true
      }
    });
    if expired > 0 {
      self.count_shed(expired);
      tracing::warn!(
        "[transport-ws] Clear-retry budget lapsed, dropping {expired} staged command(s)"
      );
    }
  }

  /// Queue one disconnect clear, retrying while `deadline` holds.
  ///
  /// Survives a transiently full sink where a single attempt would shed.
  /// Used by the shutdown drain, where waiting (bounded) is acceptable;
  /// the live pump stages instead and never waits.
  pub(crate) async fn emit_clear_retry(
    &self,
    cmd: ActivityCmd,
    deadline: std::time::Instant,
  ) -> bool {
    let mut pending = Some(cmd);
    while let Some(cmd) = pending.take() {
      match self.tx.try_send(cmd) {
        Ok(()) => return true,
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
          self.dropped.fetch_add(1, Ordering::Relaxed);
          tracing::warn!("[transport-ws] Event sink closed, dropping clear");
          return false;
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(cmd)) => {
          if std::time::Instant::now() >= deadline {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("[transport-ws] Event sink still full, dropping clear");
            return false;
          }
          pending = Some(cmd);
          tokio::time::sleep(Duration::from_millis(1)).await;
        }
      }
    }
    // `pending` is `Some` on entry: the loop always returns above.
    false
  }
}

/// Shareable read handle: snapshots and counters without transport ownership.
///
/// `Clone + Send + Sync`: hand to telemetry tasks freely.
#[derive(Debug, Clone)]
pub struct TransportHandle {
  clients: Arc<RwLock<FxHashMap<ClientId, ClientSlot>>>,
  dropped: Arc<AtomicU64>,
}

impl TransportHandle {
  /// Current game-client count (short read lock, never stalls).
  pub async fn client_count(&self) -> usize {
    self.clients.read().await.len()
  }

  /// Commands shed by a full/closed sink since bind.
  #[must_use]
  pub fn dropped_total(&self) -> u64 {
    self.dropped.load(Ordering::Relaxed)
  }
}

/// Discord-facing game WebSocket transport.
///
/// Binds the first free loopback port in range, pumps validated game
/// commands into the sink, and answers lock-step replies. Shut down with
/// [`shutdown`](Self::shutdown); dropping aborts the pump (server tasks
/// wind down with the runtime).
pub struct WsTransport {
  server: Option<adsum_ws::Server>,
  pump: Option<JoinHandle<()>>,
  handle: TransportHandle,
  /// Monotonic live-client total for telemetry census (the clients map
  /// stays the dispatch source of truth; this counter is O(1) to read).
  total: Arc<AtomicU64>,
  /// Retained sink sender for census queue-depth sampling.
  sink_tx: mpsc::Sender<ActivityCmd>,
  /// Staged disconnect clears (see `MAX_PENDING_COMMANDS`): flushed
  /// opportunistically on every pump event and drained on shutdown.
  pending_clears: Arc<Mutex<PendingQueue>>,
  bound_port: u16,
}

impl std::fmt::Debug for WsTransport {
  /// Bound port only; internal handles stay out of logs.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WsTransport")
      .field("bound_port", &self.bound_port)
      .finish_non_exhaustive()
  }
}

impl WsTransport {
  /// Bind the first free loopback port in `config` range and start pumping.
  ///
  /// Returns the transport plus the bounded sink receiver. Must be called
  /// within a Tokio runtime.
  ///
  /// # Errors
  ///
  /// [`AdsumError::InvalidConfig`] for zero bounds, [`AdsumError::WsBind`]
  /// (keeping the last `io::Error`) when every port is taken,
  /// [`AdsumError::WsExhausted`] when the range yields no bindable port.
  pub async fn bind(
    config: WsTransportConfig,
    user: Arc<Mutex<RpcUser>>,
  ) -> Result<(Self, mpsc::Receiver<ActivityCmd>)> {
    if config.event_queue == 0 {
      return Err(AdsumError::InvalidConfig("event_queue must be non-zero"));
    }
    if config.max_connections == 0 {
      return Err(AdsumError::InvalidConfig(
        "max_connections must be non-zero",
      ));
    }
    if config.per_client_queue == 0 {
      return Err(AdsumError::InvalidConfig(
        "per_client_queue must be non-zero",
      ));
    }

    let mut last_err = None;
    let mut bound: Option<(adsum_ws::Server, EventHub, u16)> = None;
    for port in config.port_start..=config.port_end {
      let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
      let ws_config = adsum_ws::ServerConfig::builder(addr)
        .max_connections(config.max_connections)
        .event_queue(config.event_queue)
        .per_client_queue(config.per_client_queue)
        .max_message_size(config.max_message_size)
        .max_frame_size(config.max_frame_size)
        .build()
        .map_err(|_| AdsumError::InvalidConfig("adsum-ws bounds must be non-zero"))?;
      match adsum_ws::Server::bind(ws_config).await {
        Ok((server, hub)) => {
          // Port 0 asks the OS: report what we actually hold.
          let actual = server.local_addr().port();
          tracing::info!("[transport-ws] Game server on port {actual}");
          bound = Some((server, hub, actual));
          break;
        }
        Err(adsum_ws::Error::Bind(source)) => {
          if source.kind() == std::io::ErrorKind::AddrInUse {
            tracing::warn!("[transport-ws] Port {port} in use, trying next");
          } else {
            tracing::warn!("[transport-ws] Cannot bind port {port} ({source}), trying next");
          }
          last_err = Some(source);
        }
        // Handshake/config failures cannot happen pre-accept; treat as fatal
        // for this port and continue the scan.
        Err(other) => {
          tracing::warn!("[transport-ws] Failed to start server on port {port}: {other}");
        }
      }
    }

    let (server, hub, bound_port) = match bound {
      Some(bound) => bound,
      None => {
        tracing::error!("[transport-ws] Failed to bind any port");
        return match last_err {
          Some(source) => Err(AdsumError::WsBind {
            start: config.port_start,
            end: config.port_end,
            source,
          }),
          None => Err(AdsumError::WsExhausted {
            start: config.port_start,
            end: config.port_end,
          }),
        };
      }
    };

    let (tx, rx) = mpsc::channel(config.event_queue);
    let clients = Arc::new(RwLock::new(FxHashMap::default()));
    let dropped = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let pending_clears = Arc::new(Mutex::new(PendingQueue::new()));
    let pump = tokio::spawn(pump_loop(PumpCtx {
      hub,
      clients: Arc::clone(&clients),
      total: Arc::clone(&total),
      sink: Sink {
        tx: tx.clone(),
        dropped: Arc::clone(&dropped),
      },
      pending_clears: Arc::clone(&pending_clears),
      user,
      set_activity: config.set_activity,
      secondary_events: config.secondary_events,
    }));

    let handle = TransportHandle { clients, dropped };
    Ok((
      Self {
        server: Some(server),
        pump: Some(pump),
        handle: handle.clone(),
        total: Arc::clone(&total),
        sink_tx: tx,
        pending_clears,
        bound_port,
      },
      rx,
    ))
  }

  /// Actual bound port (differs from config when port `0` was requested).
  #[must_use]
  pub fn bound_port(&self) -> u16 {
    self.bound_port
  }

  /// Shareable read handle for telemetry.
  #[must_use]
  pub fn handle(&self) -> TransportHandle {
    self.handle.clone()
  }

  /// Live game-client total for the telemetry census (O(1) atomic read).
  #[must_use]
  pub fn client_total(&self) -> Arc<AtomicU64> {
    Arc::clone(&self.total)
  }

  /// Shared sink sender for census queue-depth sampling.
  #[must_use]
  pub fn sink_sender(&self) -> mpsc::Sender<ActivityCmd> {
    self.sink_tx.clone()
  }

  /// Current game-client count (short read lock, never stalls).
  pub async fn client_count(&self) -> usize {
    self.handle.client_count().await
  }

  /// Commands shed by a full/closed sink since bind.
  #[must_use]
  pub fn dropped_total(&self) -> u64 {
    self.handle.dropped_total()
  }

  /// Graceful shutdown: close the listener and connections, drain the pump
  /// (including disconnect clears for every tracked pid), then clear any
  /// residue the pump never saw (tasks aborted without `Disconnect`).
  /// This is the only path that cleans up publications: `Drop` merely
  /// aborts the pump (Rust forbids `await` there), so owners must call
  /// `shutdown()` — the daemon always does. Past-budget sheds stay bounded
  /// downstream by ghost reaping.
  pub async fn shutdown(mut self) {
    if let Some(server) = self.server.take() {
      server.shutdown().await;
    }
    if let Some(pump) = self.pump.take() {
      let _ = pump.await;
    }
    // Belt and braces: connection tasks aborted without `Disconnect` (or
    // prunes the pump never processed) leave tracked pids with no one left
    // to clear them. The pump is gone, so stage them into the shared queue
    // and drain it here. Normally empty: `Disconnect` already cleared
    // everything on the way down.
    let sink = Sink {
      tx: self.sink_tx.clone(),
      dropped: Arc::clone(&self.handle.dropped),
    };
    {
      let mut clients = self.handle.clients.write().await;
      for (_, slot) in clients.drain() {
        self.total.fetch_sub(1, Ordering::Relaxed);
        for published in &slot.published {
          sink.stage_clear(&self.pending_clears, published);
        }
      }
    }
    // Opportunistic flush first (instant when healthy), then bounded retry
    // per entry: each clear carries its own deadline, so this loop is
    // bounded by the youngest entry instead of stalling shutdown on a
    // wedged bridge. Past-budget leftovers shed counted.
    sink.flush_pending(&self.pending_clears);
    // Drain in batches: one lock per batch instead of one per entry, and
    // the shared queue is empty between batches so a late staging cannot
    // be stranded by a walk that already passed it.
    loop {
      let batch = take_pending(&self.pending_clears);
      if batch.is_empty() {
        break;
      }
      for entry in batch {
        sink
          .emit_clear_retry(entry.payload.into_cmd(), entry.deadline)
          .await;
      }
    }
  }
}

/// Take the whole staged queue at once, leaving an empty one behind.
///
/// The shutdown drain used to lock, pop one entry, unlock, await that
/// entry's bounded retry, and repeat — one lock acquisition per entry and
/// a queue that kept shifting under each pop. Taking it in one lock costs
/// the same total work and leaves the shared queue empty, so a late
/// staging is never stranded by a drain that already walked past it.
fn take_pending(pending: &Mutex<PendingQueue>) -> PendingQueue {
  std::mem::take(&mut *pending.lock().unwrap_or_else(|e| e.into_inner()))
}

impl Drop for WsTransport {
  /// Abort a still-running pump so drops never park it forever.
  fn drop(&mut self) {
    // Best-effort: an explicit shutdown() drains gracefully; a drop must
    // at least not leave the pump parked forever.
    if let Some(pump) = &self.pump {
      pump.abort();
    }
  }
}

/// Pump-owned context (moved into the pump task).
struct PumpCtx {
  hub: EventHub,
  clients: Arc<RwLock<FxHashMap<ClientId, ClientSlot>>>,
  total: Arc<AtomicU64>,
  sink: Sink,
  /// Staged disconnect clears (see `MAX_PENDING_COMMANDS`).
  pending_clears: Arc<Mutex<PendingQueue>>,
  user: Arc<Mutex<RpcUser>>,
  set_activity: bool,
  secondary_events: bool,
}

/// Event pump: the only task touching the map, one short guard per event.
async fn pump_loop(
  PumpCtx {
    mut hub,
    clients,
    total,
    sink,
    pending_clears,
    user,
    set_activity,
    secondary_events,
  }: PumpCtx,
) {
  while let Some(event) = hub.next_event().await {
    match event {
      Event::Connect(id, responder) => {
        on_connect(id, responder, &clients, &total, &user).await;
      }
      Event::Disconnect(id, _) => {
        remove_and_clear(id, &clients, &total, &sink, &pending_clears).await;
      }
      Event::Message(id, message) => {
        on_message(
          id,
          message,
          &clients,
          &total,
          &sink,
          &pending_clears,
          &user,
          set_activity,
          secondary_events,
        )
        .await;
      }
      // `Event` is non-exhaustive: future variants must not break the pump.
      _ => {}
    }
  }
}

/// Validate a new game client (version, encoding, origin), send READY and
/// register its slot. Rejected clients are closed before registration.
async fn on_connect(
  id: ClientId,
  responder: Responder,
  clients: &Arc<RwLock<FxHashMap<ClientId, ClientSlot>>>,
  total: &Arc<AtomicU64>,
  user: &Arc<Mutex<RpcUser>>,
) {
  // Parse + validate before any reply or insert.
  let uri = responder.details().uri.as_ref();
  let version = query_param(uri, "v").unwrap_or("0");
  let encoding = query_param(uri, "encoding").unwrap_or("json");
  // The connection's fallback id is the one value that must outlive the
  // handshake, so it enters shared ownership once here — one copy into
  // the refcounted layout per connection, and every later clone (the
  // per-message snapshot included) is free.
  let query_client_id = query_param(uri, "client_id").map(AppId::from);
  if version != "1" || encoding != "json" {
    tracing::warn!("[transport-ws] Rejecting client {id} (v={version}, encoding={encoding})");
    responder.close(CloseCode::Normal).await;
    return;
  }
  // Origins are per-connection: refuse mismatches here, before any READY
  // or slot registration, instead of per message downstream.
  if !origin_allowed(
    responder
      .details()
      .headers
      .get("origin")
      .and_then(|v| v.to_str().ok()),
  ) {
    tracing::warn!("[transport-ws] Refused origin for client {id}");
    responder.close(CloseCode::Normal).await;
    return;
  }

  // Snapshot the identity under a short lock; the guard never crosses await.
  let ready = user
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .ready_payload();
  if responder
    .send_async(Message::Text(ready.into()))
    .await
    .is_err()
  {
    // Vanished mid-handshake: nothing to track.
    return;
  }
  clients.write().await.insert(
    id,
    ClientSlot {
      responder,
      published: Vec::new(),
      query_client_id,
    },
  );
  total.fetch_add(1, Ordering::Relaxed);
}

/// Remove the slot and emit one clear per published pid (shared by
/// Disconnect and prune paths). A connection that published several pids
/// (multiplexing companion) clears every card, not just the latest.
/// Complete by construction: extras past the bound are refused before
/// forwarding, so everything forwarded is tracked here. Each clear retries
/// within one shared budget (never one timeout per pid), so a transiently
/// wedged bridge still gets its clears without stalling the pump past a
/// second; past the budget clears shed counted (bridge ghost-reaping
/// bounds any residual staleness).
async fn remove_and_clear(
  id: ClientId,
  clients: &Arc<RwLock<FxHashMap<ClientId, ClientSlot>>>,
  total: &Arc<AtomicU64>,
  sink: &Sink,
  pending_clears: &Arc<Mutex<PendingQueue>>,
) {
  let slot = clients.write().await.remove(&id);
  let Some(slot) = slot else { return };
  total.fetch_sub(1, Ordering::Relaxed);
  // Older staged clears first (causality), then this slot's own clears.
  // Each stages instead of shedding on a full sink: the next pump event,
  // the retry below, or the shutdown drain retries them until delivered
  // or closed.
  sink.flush_pending(pending_clears);
  let mut staged_any = false;
  for published in &slot.published {
    let cmd = handlers::clear_for_slot(published);
    match sink.try_emit(cmd) {
      TryEmit::Sent => {}
      TryEmit::Full(_) => {
        // Staged slim from the slot, not from the refused command: only
        // the metadata is retained while the sink stays full.
        sink.stage_clear(pending_clears, published);
        staged_any = true;
      }
      TryEmit::Closed => {
        sink.count_shed(1);
        tracing::warn!("[transport-ws] Event sink closed, dropping clear");
      }
    }
  }
  // No further traffic may ever come: keep flushing in the background
  // until empty, closed, or budget lapse (see `spawn_clear_retry`).
  if staged_any {
    spawn_clear_retry(sink.clone(), Arc::clone(pending_clears));
  }
}

/// Retry staged clears without further traffic: polled flush until empty
/// (a closed sink drains counted inside the flush). Expiry is per entry
/// (see `expire_pending`), so overlapping tasks never shed each other's
/// clears, and every task ends on its own — no traffic pattern leaks
/// tasks. Complements the message-path and shutdown-drain flushes, which
/// cover traffic and exit.
fn spawn_clear_retry(sink: Sink, pending_clears: Arc<Mutex<PendingQueue>>) {
  tokio::spawn(async move {
    loop {
      sink.flush_pending(&pending_clears);
      sink.expire_pending(&pending_clears);
      if pending_clears
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
      {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  });
}

#[allow(clippy::too_many_arguments)]
/// Dispatch one client message; `false` means the client died and its slot
/// must be pruned (disconnect clears follow downstream).
async fn on_message(
  id: ClientId,
  message: Message,
  clients: &Arc<RwLock<FxHashMap<ClientId, ClientSlot>>>,
  total: &Arc<AtomicU64>,
  sink: &Sink,
  pending_clears: &Arc<Mutex<PendingQueue>>,
  user: &Arc<Mutex<RpcUser>>,
  set_activity: bool,
  secondary_events: bool,
) {
  // Staged disconnect clears ride first (causality: older clears before
  // new publishes). Never waits: a still-full sink simply keeps them
  // staged for the next event.
  sink.flush_pending(pending_clears);
  // Snapshot the cheap fields; the read guard drops before any await
  // below (and the deep `last_cmd` is never cloned here).
  let slot = match clients.read().await.get(&id) {
    Some(slot) => SlotSnapshot::from_slot(slot),
    None => {
      // Stale message for a removed slot (pruned or rejected client).
      return;
    }
  };
  let Message::Text(text) = message else {
    tracing::warn!("[transport-ws] Ignoring non-text message from client {id}");
    return;
  };
  let event: ActivityCmd = match serde_json::from_str(&text) {
    Ok(event) => event,
    Err(err) => {
      tracing::warn!("[transport-ws] Invalid message from client {id}: {err}");
      return;
    }
  };

  // Every arm reports liveness: `false` means the game client died without
  // a clean `Disconnect` and its slot must be released now.
  //
  // A `SET_ACTIVITY` with `args` but no `activity` field is malformed, not
  // a clear: reject with the official error before tracking or forwarding.
  // Missing `args` keeps its legacy silent path (forwarded, bridge drops).
  if set_activity
    && event.cmd == "SET_ACTIVITY"
    && event
      .args
      .as_ref()
      .is_some_and(|args| args.activity.is_none())
  {
    let alive = handlers::handle_missing_activity(&event, &slot.responder).await;
    if !alive {
      tracing::info!("[transport-ws] Client {id} send failed, pruning");
      remove_and_clear(id, clients, total, sink, pending_clears).await;
    }
    return;
  }

  // Two passes, because `SET_ACTIVITY` now consumes the command by value
  // (it owns data the sink needs) while every other arm only borrows it.
  // Matching directly on `event.cmd.as_str()` would hold a borrow of
  // `event` for the whole match and forbid the move, so the value-moving
  // arm is selected first and the rest keeps its borrow path.
  let consumes_event = event.cmd == "SET_ACTIVITY" && set_activity;
  let alive = if consumes_event {
    // Genuine clears are not publications: they never consume history,
    // always forward (the bridge may hold the card), and free the slot
    // once delivered so new pids fit again. Applies the same
    // connect-query client_id fallback the forwarded command carries.
    let app_id = event
      .application_id
      .clone()
      .or_else(|| slot.query_client_id.clone());
    let pid = event.args.as_ref().and_then(|a| a.pid).unwrap_or_default();
    let is_clear = match event.args.as_ref() {
      // Only an explicit `activity: null` is a clear; a missing field was
      // rejected above and never reaches here.
      Some(args) => matches!(args.activity, Some(None)),
      // No `args` at all keeps the legacy clear path: never track pid 0.
      None => true,
    };
    // Record first (one short write, no await inside): every forwarded
    // publication is tracked, so disconnect cleanup stays complete. The
    // pump is the only map mutator, so admission cannot change before
    // forwarding below.
    let admission = if is_clear {
      None
    } else if let Some(entry) = clients.write().await.get_mut(&id) {
      Some(entry.note_published(app_id, pid, event.nonce.clone()))
    } else {
      // Slot pruned mid-flight: forward nothing rather than ghost.
      None
    };
    // Refusals still earn their lock-step reply (clients hang without
    // one) but never reach the sink — recording the publication
    // regardless also keeps the reply-fails prune below clearing the
    // pid instead of ghosting it. Publications behind a still-full
    // staged queue wait their turn instead of overtaking older clears.
    let backlog = !pending_clears
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .is_empty();
    // A still-full staged queue serializes everything behind it —
    // including genuine clears, so a clear for a staged pid lands after
    // its publish instead of jumping ahead (same-pid inversion would
    // show-then-clear out of order downstream).
    let forward = if is_clear && !backlog {
      handlers::Forward::Direct
    } else if is_clear {
      handlers::Forward::Staged
    } else {
      match admission {
        Some(Admission::Admitted { .. }) if backlog => handlers::Forward::Staged,
        Some(Admission::Admitted { .. }) => handlers::Forward::Direct,
        _ => handlers::Forward::Refused,
      }
    };
    let (alive, delivered) = handlers::handle_set_activity(
      event,
      slot.query_client_id.as_ref().map(|id| id.as_ref()),
      &slot.responder,
      sink,
      pending_clears,
      forward,
    )
    .await;
    if delivered {
      // Landed: genuine clears free their slot for new pids.
      if is_clear && let Some(entry) = clients.write().await.get_mut(&id) {
        entry.forget_published(pid);
      }
    } else {
      match admission {
        // Shed publish: roll back (a refresh restores its prior slot so
        // shown cards stay tracked; first-time publishes leave nothing).
        Some(Admission::Admitted { displaced }) => {
          if let Some(entry) = clients.write().await.get_mut(&id) {
            entry.rollback_publish(pid, displaced);
          }
        }
        Some(Admission::RefusedFull) => {
          tracing::debug!(
            "[transport-ws] Client {id} past {MAX} tracked pids, refusing pid {pid}",
            MAX = handlers::MAX_TRACKED_PIDS,
          );
        }
        None => {}
      }
    }
    alive
  } else {
    // `SET_ACTIVITY` with the feature off stays silent, exactly as before.
    if event.cmd == "SET_ACTIVITY" {
      return;
    }
    match event.cmd.as_str() {
      "INVITE_BROWSER" | "GUILD_TEMPLATE_BROWSER" | "GIFT_CODE_BROWSER" => {
        if !secondary_events {
          return;
        }
        // By value: the handler echoes by borrowing and forwards the
        // command itself, so nothing here is deep-copied.
        handlers::handle_browser_command(event, &slot.responder, sink).await
      }
      "DEEP_LINK" => {
        if !secondary_events {
          return;
        }
        handlers::handle_deep_link(&event, &slot.responder).await
      }
      "CONNECTIONS_CALLBACK" => {
        if !secondary_events {
          return;
        }
        handlers::handle_connections_callback(&event, &slot.responder).await
      }
      "SUBSCRIBE" | "UNSUBSCRIBE" => handlers::handle_subscribe(&event, &slot.responder).await,
      "GET_USER" => {
        // Serialize under the guard, then drop it before the await: a std
        // Mutex must never be held across a suspension point. The user is
        // borrowed, so the reply is serialized straight from the shared
        // value — the snapshot this replaces deep-copied every field of
        // `RpcUser` on every GET_USER.
        let response = {
          let user = user.lock().unwrap_or_else(|e| e.into_inner());
          handlers::get_user_response(&event, &user)
        };
        let reply = Message::Text(response.into());
        handlers::reply(&slot.responder, reply).await
      }
      other => handlers::handle_unknown(other, &event, &slot.responder).await,
    }
  };
  if !alive {
    tracing::info!("[transport-ws] Client {id} send failed, pruning");
    remove_and_clear(id, clients, total, sink, pending_clears).await;
  }
}

/// Browser origins must be Discord; absent origin (native clients) passes.
fn origin_allowed(origin: Option<&str>) -> bool {
  match origin {
    None => true,
    Some(value) => ALLOWED_ORIGINS.contains(&value),
  }
}

#[cfg(test)]
#[path = "../tests/unit/transport/mod.rs"]
mod tests;
