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
mod tests {
  use super::*;

  /// A briefly-full sink still delivers via retry (no shed on transients).
  #[tokio::test]
  async fn clear_retry_survives_a_briefly_full_sink() {
    // Cap-1 sink held full: the retry lands once space frees inside the
    // budget (a single 250ms attempt would shed it).
    let (tx, mut rx) = mpsc::channel(1);
    tx.send_timeout(
      handlers::clear_for_slot(&handlers::PublishedSlot {
        app_id: None,
        pid: 1,
        nonce: Value::Null,
      }),
      SINK_SEND_TIMEOUT,
    )
    .await
    .expect("filler");
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let cmd = handlers::clear_for_slot(&handlers::PublishedSlot {
      app_id: None,
      pid: 7,
      nonce: Value::Null,
    });
    // Drain once after 400ms, then hold the receiver open (dropping it
    // would close the channel and turn the retry into a `Closed` shed).
    let drain = tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(400)).await;
      rx.recv().await.expect("filler");
      std::future::pending::<()>().await;
    });
    let deadline = std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET;
    assert!(sink.emit_clear_retry(cmd, deadline).await);
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
    drain.abort();
  }

  /// A closed sink fails fast-ish and counts the shed clear.
  #[tokio::test]
  async fn clear_retry_gives_up_past_the_budget() {
    // Closed sink: no capacity will ever return — fail fast-ish, counted.
    let (tx, rx) = mpsc::channel::<ActivityCmd>(1);
    drop(rx);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let cmd = handlers::clear_for_slot(&handlers::PublishedSlot {
      app_id: None,
      pid: 7,
      nonce: Value::Null,
    });
    let deadline = std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET;
    assert!(!sink.emit_clear_retry(cmd, deadline).await);
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
  }

  /// An activity with a sizable `details`, so a clear that still carried
  /// one would dominate the entry's size.
  fn activity_with_details() -> adsum_types::cmd::Activity {
    adsum_types::cmd::Activity {
      id: None,
      name: Some("G".to_string()),
      buttons: None,
      r#type: 0,
      url: None,
      created_at: None,
      session_id: None,
      platform: None,
      supported_platforms: None,
      timestamps: None,
      application_id: None,
      details: Some("d".repeat(1024)),
      details_url: None,
      state: None,
      state_url: None,
      sync_id: None,
      instance: None,
      status_display_type: None,
      flags: None,
      emoji: None,
      party: None,
      assets: None,
      secrets: None,
      metadata: None,
      extra: Default::default(),
    }
  }

  /// One published slot, as a disconnect would stage it.
  fn slot(pid: u64) -> handlers::PublishedSlot {
    handlers::PublishedSlot {
      app_id: Some(AppId::from("app-1")),
      pid,
      nonce: Value::String(format!("n{pid}")),
    }
  }

  /// A `SET_ACTIVITY` big enough that a clear holding one would be
  /// measurable (the point of the slim representation).
  fn big_publish(pid: u64) -> ActivityCmd {
    ActivityCmd {
      application_id: Some(AppId::from("app-1")),
      cmd: "SET_ACTIVITY".to_string(),
      data: None,
      evt: None,
      args: Some(ActivityCmdArgs {
        pid: Some(pid),
        activity: Some(Some(activity_with_details())),
        code: None,
        user_id: None,
      }),
      nonce: Value::String(format!("n{pid}")),
    }
  }

  /// A staged clear rebuilds the exact command `clear_for_slot` produces.
  /// This is the invariant that lets the queue stay slim without changing
  /// what a wedged bridge eventually receives.
  #[test]
  fn slim_clear_rebuilds_the_same_command() {
    for pid in [0u64, 7, 42, u64::MAX] {
      let slot = slot(pid);
      let slim = PendingClear::from_slot(&slot);
      // JSON, not a struct compare: a changed field order or a dropped
      // field on either side shows up here.
      assert_eq!(
        serde_json::to_string(&slim.into_cmd()).expect("slim clear"),
        serde_json::to_string(&handlers::clear_for_slot(&slot)).expect("direct clear"),
        "pid {pid}: slim clear must rebuild byte-identically"
      );
    }
  }

  /// The slim form is a faithful inverse of the command it rebuilds, in
  /// both directions. The flush relies on it: a refused clear is put back
  /// as metadata taken from the command the sink handed it, so a clear that
  /// survives any number of refusals must still rebuild the same bytes.
  #[test]
  fn slim_clear_round_trips_through_its_command() {
    for pid in [0u64, 7, 42, u64::MAX] {
      let slot = slot(pid);
      let slim = PendingClear::from_slot(&slot);
      let cmd = slim.clone().into_cmd();
      let back = PendingClear::from_cmd(&cmd);
      assert_eq!(back, slim, "pid {pid}: the slim form must round-trip");
    }
    // A nonce that is not null, and an id that is present: the inverse
    // reads both back out of the command rather than assuming the empty
    // slot.
    let slot = handlers::PublishedSlot {
      app_id: Some(adsum_types::AppId::from("app-7")),
      pid: 9,
      nonce: Value::String("n9".to_string()),
    };
    let slim = PendingClear::from_slot(&slot);
    let cmd = slim.clone().into_cmd();
    assert_eq!(PendingClear::from_cmd(&cmd), slim);
  }

  /// A staged clear is materially smaller than the command it replaces:
  /// the whole point of holding slim metadata instead of the full command.
  #[test]
  fn slim_clear_is_smaller_than_the_command() {
    let cmd = big_publish(7);
    let clear = PendingClear::from_slot(&slot(7)).into_cmd();
    assert!(
      std::mem::size_of::<PendingClear>() < std::mem::size_of::<ActivityCmd>(),
      "metadata ({}) must be smaller than the command ({})",
      std::mem::size_of::<PendingClear>(),
      std::mem::size_of::<ActivityCmd>()
    );
    // And the activity the publish carried is genuinely gone.
    assert!(
      cmd
        .args
        .as_ref()
        .and_then(|a| a.activity.as_ref())
        .and_then(|a| a.as_ref())
        .is_some()
    );
    assert!(
      clear
        .args
        .as_ref()
        .is_some_and(|a| matches!(a.activity, Some(None)))
    );
  }

  /// Staged entries never exceed the bound; every shed is counted.
  #[test]
  fn staged_entries_stay_bounded_and_counted() {
    let (tx, _rx) = mpsc::channel::<ActivityCmd>(1);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    for pid in 0..(MAX_PENDING_COMMANDS + 5) as u64 {
      sink.stage_clear(&pending, &slot(pid));
    }
    // Bounded history, and every shed entry counted (never silent).
    assert!(pending.lock().unwrap().len() <= MAX_PENDING_COMMANDS);
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 5);
  }

  /// Publishes stage too, so a wedged sink parks them instead of dropping.
  #[test]
  fn staged_publish_keeps_its_command() {
    let (tx, _rx) = mpsc::channel::<ActivityCmd>(1);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    sink.stage_publish(&pending, big_publish(5));
    pending
      .lock()
      .unwrap()
      .retain(|entry| matches!(entry.payload, PendingPayload::Publish(_)));
    assert_eq!(pending.lock().unwrap().len(), 1);
  }

  /// Flush delivers oldest-first and stops at the first full queue.
  #[tokio::test]
  async fn flush_preserves_order_and_stops_when_full() {
    // Cap-1 sink held full by a filler: flush delivers nothing, staged intact.
    let (tx, mut rx) = mpsc::channel(1);
    tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
      .await
      .expect("filler");
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    for pid in [7u64, 8, 9] {
      sink.stage_clear(&pending, &slot(pid));
    }
    sink.flush_pending(&pending);
    assert_eq!(pending.lock().unwrap().len(), 3);
    // Free slots one by one: staged entries land oldest-first.
    for pid in [1u64, 7, 8] {
      let cmd = rx.try_recv().expect("slot frees in order");
      assert_eq!(cmd.args.as_ref().and_then(|a| a.pid), Some(pid));
      sink.flush_pending(&pending);
    }
    assert!(pending.lock().unwrap().is_empty());
    let last = rx.try_recv().expect("final staged entry");
    assert_eq!(last.args.as_ref().and_then(|a| a.pid), Some(9));
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
  }

  /// A clear a full sink refuses keeps its slim form when it goes back.
  ///
  /// The flush offers each entry as a full command, so a refused clear used
  /// to come back as a boxed publish: same bytes, same place in the queue,
  /// but the ~1 KB command retained per entry for as long as the sink stays
  /// full. A wedged sink is exactly the case the slim form exists for, and
  /// it runs on every pump event while the queue is non-empty.
  #[tokio::test]
  async fn a_refused_clear_is_requeued_slim() {
    // Cap-1 sink held full by a filler: the flush delivers nothing, so
    // every staged entry takes the refused path.
    let (tx, _rx) = mpsc::channel(1);
    tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
      .await
      .expect("filler");
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    for pid in [7u64, 8] {
      sink.stage_clear(&pending, &slot(pid));
    }
    // Each entry carries its own retry window; the requeue must not stamp
    // a new one, or the entry the oldest retry task is waiting on silently
    // keeps a stranger's window.
    let deadlines: Vec<_> = pending.lock().unwrap().iter().map(|e| e.deadline).collect();

    sink.flush_pending(&pending);

    let pending = pending.lock().unwrap();
    assert_eq!(pending.len(), 2, "a full queue keeps every entry");
    let pids: Vec<u64> = pending
      .iter()
      .map(|entry| match &entry.payload {
        PendingPayload::Clear(clear) => clear.pid,
        PendingPayload::Publish(_) => {
          panic!("a refused clear came back as a publish, retaining the full command")
        }
      })
      .collect();
    assert_eq!(
      pids,
      vec![7, 8],
      "both clears stay slim and keep their order"
    );
    assert_eq!(
      deadlines,
      pending.iter().map(|e| e.deadline).collect::<Vec<_>>(),
      "the requeue keeps each entry's own deadline"
    );
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
  }

  /// Expiry removes only lapsed entries, keeping fresh retry windows.
  #[test]
  fn expiry_sheds_only_lapsed_entries() {
    let (tx, _rx) = mpsc::channel::<ActivityCmd>(1);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    // One long-lapsed entry and one fresh entry, built directly (staging
    // always stamps `now + budget`, which no deterministic test can outwait).
    let past = std::time::Instant::now() - Duration::from_secs(1);
    let future = std::time::Instant::now() + DISCONNECT_CLEAR_BUDGET;
    for (pid, deadline) in [(1u64, past), (2u64, future)] {
      pending.lock().unwrap().push_back(PendingEntry {
        payload: PendingPayload::Clear(PendingClear::from_slot(&slot(pid))),
        deadline,
      });
    }
    sink.expire_pending(&pending);
    // Only the lapsed entry shed (counted); the fresh one survives with
    // its own retry window intact.
    let pending = pending.lock().unwrap();
    assert_eq!(pending.len(), 1);
    let PendingPayload::Clear(clear) = &pending[0].payload else {
      panic!("expected a clear");
    };
    assert_eq!(clear.pid, 2);
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
  }

  /// Discord origins pass, missing origin passes, anything else is refused.
  #[test]
  fn origin_policy() {
    assert!(origin_allowed(None));
    assert!(origin_allowed(Some("https://discord.com")));
    assert!(origin_allowed(Some("https://canary.discord.com")));
    assert!(origin_allowed(Some("https://ptb.discord.com")));
    assert!(!origin_allowed(Some("https://evil.example")));
    assert!(!origin_allowed(Some("")));
  }

  /// The re-exported query parser keeps its legacy key/value behavior.
  #[test]
  fn query_params_match_legacy_semantics() {
    // Canonical parser lives in adsum-protocol (tested there); this pins
    // the import still resolves to the same behavior here.
    let uri = "/?v=1&client_id=abc";
    assert_eq!(query_param(uri, "v"), Some("1"));
    assert_eq!(query_param(uri, "client_id"), Some("abc"));
  }

  /// A4: the per-message snapshot shares the connection's client id
  /// instead of copying it.
  ///
  /// The snapshot is rebuilt for **every** message — `SUBSCRIBE` and
  /// `GET_USER` included, which never use the id at all — so an owned
  /// `String` here paid a deep copy on the whole message path. With the
  /// id refcounted the clone is a counter bump and the buffer is shared.
  /// Goes through [`SlotSnapshot::from_slot`] on purpose: building the
  /// struct by hand here would leave the real clone site untested, and
  /// a copy reintroduced there would pass.
  #[test]
  fn slot_snapshot_shares_the_connection_client_id() {
    let slot = ClientSlot {
      responder: Responder::for_test(),
      published: Vec::new(),
      query_client_id: Some(AppId::from("123456789012345678")),
    };
    let stored_ptr = slot
      .query_client_id
      .as_ref()
      .expect("client id")
      .as_ref()
      .as_ptr();

    let snapshot = SlotSnapshot::from_slot(&slot);

    let shared = snapshot.query_client_id.expect("client id");
    assert!(
      std::ptr::eq(shared.as_ref().as_ptr(), stored_ptr),
      "the per-message snapshot must share the connection id, never copy it"
    );
  }
  ///
  /// The staged path used to stage first and flush immediately, which cost
  /// two lock round-trips on the pending queue for every staged publish —
  /// including the common case where the sink had room and the entry left
  /// the queue in the very next statement.
  #[tokio::test]
  async fn a_publish_with_room_is_delivered_not_queued() {
    let (tx, mut rx) = mpsc::channel(4);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());

    assert!(sink.emit_or_stage_publish(&pending, big_publish(3)));

    assert!(
      pending.lock().unwrap().is_empty(),
      "a deliverable publish must not sit in the queue"
    );
    let got = rx.try_recv().expect("delivered to the sink");
    assert_eq!(got.args.as_ref().and_then(|a| a.pid), Some(3));
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
  }

  /// A full sink still stages, and the ordering invariant holds: the entry
  /// joins the queue behind whatever the earlier flush could not drain.
  #[tokio::test]
  async fn a_full_sink_stages_the_publish_in_order() {
    // Cap-1 sink held full by a filler: nothing can be delivered.
    let (tx, mut rx) = mpsc::channel(1);
    tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
      .await
      .expect("filler");
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    // An older entry the earlier flush left behind (sink full).
    sink.stage_clear(&pending, &slot(7));

    assert!(sink.emit_or_stage_publish(&pending, big_publish(9)));

    assert_eq!(
      pending.lock().unwrap().len(),
      2,
      "both entries stay queued while the sink is full"
    );
    // Free the sink: the older entry must land first.
    rx.try_recv().expect("filler drains");
    sink.flush_pending(&pending);
    let first = rx.try_recv().expect("older entry first");
    assert_eq!(
      first.args.as_ref().and_then(|a| a.pid),
      Some(7),
      "a new publish must never overtake an older staged entry"
    );
  }

  /// A gone sink counts the shed and reports failure, like every other
  /// emit path.
  #[tokio::test]
  async fn a_closed_sink_sheds_and_reports_failure() {
    let (tx, rx) = mpsc::channel::<ActivityCmd>(1);
    drop(rx);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());

    assert!(!sink.emit_or_stage_publish(&pending, big_publish(3)));
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 1);
    assert!(pending.lock().unwrap().is_empty());
  }
  /// A deliverable publish must not touch the staged queue at all.
  ///
  /// The outcome alone does not prove it — staging and immediately
  /// draining leaves the queue empty either way. What differs is the
  /// collateral: at the cap, staging sheds the oldest staged entry, which
  /// was still owed delivery. Delivering directly leaves it queued.
  #[tokio::test]
  async fn a_deliverable_publish_leaves_the_queue_untouched() {
    // Sink starts full so the older entries can be staged, then frees up
    // without a flush: the queue sits at its cap, deliverable but queued.
    let (tx, mut rx) = mpsc::channel(1);
    tx.send_timeout(handlers::clear_for_slot(&slot(1)), SINK_SEND_TIMEOUT)
      .await
      .expect("filler");
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    for pid in 0..MAX_PENDING_COMMANDS as u64 {
      sink.stage_clear(&pending, &slot(pid));
    }
    assert_eq!(pending.lock().unwrap().len(), MAX_PENDING_COMMANDS);
    rx.try_recv().expect("free the sink");
    // No flush here, deliberately: the queue is full and the sink has room.

    assert!(sink.emit_or_stage_publish(&pending, big_publish(99)));

    let got = rx.try_recv().expect("the publish is delivered directly");
    assert_eq!(got.args.as_ref().and_then(|a| a.pid), Some(99));
    assert_eq!(
      sink.dropped.load(Ordering::Relaxed),
      0,
      "a deliverable publish must not shed an older staged entry"
    );
    assert_eq!(
      pending.lock().unwrap().len(),
      MAX_PENDING_COMMANDS,
      "the staged queue must be untouched"
    );
  }
  /// N13: the shutdown drain takes the queue in batches, not one entry per
  /// lock round-trip.
  ///
  /// It used to lock, pop one entry, unlock, await its bounded retry, and
  /// repeat — so a wedge at shutdown paid one lock acquisition per entry
  /// and the queue kept shifting under each pop. Taking the whole queue
  /// leaves one lock and an empty queue behind, and the per-entry await
  /// (each clear keeps its own deadline) is unchanged.
  #[tokio::test]
  async fn a_drain_take_empties_the_queue_in_one_lock() {
    let (tx, mut rx) = mpsc::channel(8);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    for pid in [1u64, 2, 3] {
      sink.stage_clear(&pending, &slot(pid));
    }

    // One take replaces the whole queue; nothing is lost and the shared
    // queue is empty, so a concurrent stager cannot be stranded.
    let batch = take_pending(&pending);
    assert_eq!(batch.len(), 3, "the whole queue leaves in one take");
    assert!(
      pending.lock().unwrap().is_empty(),
      "the shared queue must be empty after the take"
    );
    // Deliver the batch: order is the queue's, unchanged by taking it.
    for entry in batch {
      assert!(
        sink
          .emit_clear_retry(entry.payload.into_cmd(), entry.deadline)
          .await
      );
    }
    for pid in [1u64, 2, 3] {
      let cmd = rx.try_recv().expect("drained in order");
      assert_eq!(cmd.args.as_ref().and_then(|a| a.pid), Some(pid));
    }
    assert_eq!(sink.dropped.load(Ordering::Relaxed), 0);
  }

  /// A second take after a late staging still drains it: the drain loops on
  /// batches rather than assuming the first take finished the job.
  #[tokio::test]
  async fn a_second_take_still_drains_late_entries() {
    let (tx, mut rx) = mpsc::channel(8);
    let sink = Sink {
      tx,
      dropped: Arc::new(AtomicU64::new(0)),
    };
    let pending = Mutex::new(PendingQueue::new());
    sink.stage_clear(&pending, &slot(1));
    assert_eq!(take_pending(&pending).len(), 1);
    assert!(pending.lock().unwrap().is_empty());

    // A straggler staged after the first batch (nothing does this today,
    // but the drain must not silently drop it).
    sink.stage_clear(&pending, &slot(9));
    let second = take_pending(&pending);
    assert_eq!(second.len(), 1, "a late entry is still drained");
    assert_eq!(
      second.front().map(|entry| match &entry.payload {
        PendingPayload::Clear(clear) => clear.pid,
        PendingPayload::Publish(cmd) => cmd.args.as_ref().and_then(|a| a.pid).unwrap_or_default(),
      }),
      Some(9)
    );
    let _ = rx.try_recv();
  }

  /// N12: a connection that forgets its last published pid releases the
  /// `published` buffer instead of holding its high-water mark.
  ///
  /// The list grows with what a multiplexing client publishes, so its
  /// capacity is that client's peak — not the steady state. Replacing it
  /// with a fresh `Vec` when it drains returns that buffer while the
  /// connection is still open, which is where the per-connection
  /// footprint would otherwise ratchet.
  #[test]
  fn forgetting_the_last_pid_releases_the_published_buffer() {
    let mut slot = ClientSlot {
      responder: adsum_ws::Responder::for_test(),
      published: Vec::new(),
      query_client_id: None,
    };
    for pid in 1..=handlers::MAX_TRACKED_PIDS as u64 {
      let admission = slot.note_published(
        Some(adsum_types::AppId::from("app-1")),
        pid,
        Value::String(format!("n{pid}")),
      );
      assert!(
        matches!(admission, Admission::Admitted { .. }),
        "each pid fits"
      );
    }
    let peak = slot.published.capacity();
    assert!(
      peak >= handlers::MAX_TRACKED_PIDS,
      "buffer grew to the peak"
    );

    for pid in 1..=handlers::MAX_TRACKED_PIDS as u64 {
      slot.forget_published(pid);
    }
    assert!(slot.published.is_empty());
    assert_eq!(
      slot.published.capacity(),
      0,
      "an emptied publication list must hand its buffer back"
    );
  }

  /// A list that still holds entries keeps its buffer: releasing per
  /// removal would trade a one-off retention for per-publish reallocation.
  #[test]
  fn a_partially_forgotten_list_keeps_its_buffer() {
    let mut slot = ClientSlot {
      responder: adsum_ws::Responder::for_test(),
      published: Vec::new(),
      query_client_id: None,
    };
    slot.note_published(
      Some(adsum_types::AppId::from("app-1")),
      1,
      Value::String("n1".into()),
    );
    slot.note_published(
      Some(adsum_types::AppId::from("app-1")),
      2,
      Value::String("n2".into()),
    );
    let before = slot.published.capacity();

    slot.forget_published(1);

    assert_eq!(slot.published.len(), 1);
    assert!(
      slot.published.capacity() >= before,
      "a still-populated list must not be reallocated"
    );
  }
}
