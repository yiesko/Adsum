//! Shared helpers for the server connectors: queue depth gauges, RSS
//! readings and the hourly/session resource census line.
//!
//! The gauges exist to discriminate two failure modes in long sessions
//! without guessing: an ever-growing queue depth points at producer
//! outpacing consumer (H1), while a climbing RSS with flat depths points
//! at allocator retention/fragmentation (H6). They never change runtime
//! behavior — sends and receives behave exactly like `std::mpsc`.

use std::sync::{
  atomic::{AtomicBool, AtomicUsize, Ordering},
  mpsc,
};

/// Depth gauge shared by one channel's sender(s) and receiver: incremented
/// on every successful send, decremented on every successful receive.
/// `Relaxed` is the weakest correct ordering here (diagnostic counter only:
/// no data is synchronized through it).
#[derive(Clone, Debug)]
pub struct QueueGauge {
  depth: std::sync::Arc<AtomicUsize>,
  /// Capacity-checked sheds since creation (diagnostic only).
  dropped: std::sync::Arc<AtomicUsize>,
  /// Receiver liveness: cleared when the receiver drops (`std::mpsc` has
  /// no `is_closed` probe, so the gauge tracks it instead).
  live: std::sync::Arc<AtomicBool>,
}

impl Default for QueueGauge {
  /// Fresh zeroed gauge with a live receiver slot.
  fn default() -> Self {
    Self {
      depth: std::sync::Arc::new(AtomicUsize::new(0)),
      dropped: std::sync::Arc::new(AtomicUsize::new(0)),
      live: std::sync::Arc::new(AtomicBool::new(true)),
    }
  }
}

impl QueueGauge {
  /// Fresh zeroed gauge.
  #[must_use]
  pub fn new() -> Self {
    Self::default()
  }

  /// Current backlog. It can only drift from reality if a `send`/`recv`
  /// bypasses the wrappers below — there is no such bypass: the wrapped
  /// channel ends are moved, never the raw ones.
  #[must_use]
  pub fn depth(&self) -> usize {
    self.depth.load(Ordering::Relaxed)
  }

  /// Count one queued send (diagnostic only: `Relaxed` ordering suffices).
  fn inc(&self) {
    self.depth.fetch_add(1, Ordering::Relaxed);
  }

  /// Release one backlog count after a successful receive.
  fn dec(&self) {
    self.depth.fetch_sub(1, Ordering::Relaxed);
  }

  /// Sheds since creation (capacity-checked sends only; plain `send`
  /// never sheds by construction).
  #[must_use]
  pub fn dropped_total(&self) -> usize {
    self.dropped.load(Ordering::Relaxed)
  }

  /// Count one capacity-checked shed.
  fn shed(&self) {
    self.dropped.fetch_add(1, Ordering::Relaxed);
  }

  /// Reserve one backlog slot when below `cap` (CAS loop): concurrent
  /// cloned senders can never jointly exceed cap — a loser re-reads and
  /// sheds instead of racing past the check. `Relaxed` suffices: the CAS
  /// itself serializes reservations, staleness only costs a retry.
  fn try_reserve(&self, cap: usize) -> bool {
    loop {
      let depth = self.depth.load(Ordering::Relaxed);
      if depth >= cap {
        return false;
      }
      if self
        .depth
        .compare_exchange_weak(depth, depth + 1, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
      {
        return true;
      }
    }
  }

  /// Whether the receiver is still alive (see `Drop` above).
  fn is_live(&self) -> bool {
    self.live.load(Ordering::Relaxed)
  }

  /// A fresh unbounded channel with both ends sharing one gauge.
  pub fn pair<T>() -> (GaugeSender<T>, GaugeReceiver<T>) {
    let (tx, rx) = mpsc::channel();
    let gauge = QueueGauge::new();
    (
      GaugeSender {
        inner: tx,
        gauge: gauge.clone(),
      },
      GaugeReceiver { inner: rx, gauge },
    )
  }
}

/// Outcome of a capacity-checked send (see [`GaugeSender::send_checked`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendChecked {
  /// Accepted by the channel.
  Sent,
  /// Backlog at cap: dropped and counted, by design.
  Shed,
  /// Receiver gone: the consumer is shutting down.
  Closed,
}

/// `mpsc::Sender` that counts its backlog. `Clone` shares the gauge, so
/// producers cloned across threads still report into the same depth.
pub struct GaugeSender<T> {
  inner: mpsc::Sender<T>,
  gauge: QueueGauge,
}

impl<T> Clone for GaugeSender<T> {
  /// Share the channel and gauge (same backlog counter).
  fn clone(&self) -> Self {
    Self {
      inner: self.inner.clone(),
      gauge: self.gauge.clone(),
    }
  }
}

impl<T> std::fmt::Debug for GaugeSender<T> {
  /// Backlog depth only; queued values stay out of logs.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GaugeSender")
      .field("depth", &self.gauge.depth())
      .finish_non_exhaustive()
  }
}

impl<T> GaugeSender<T> {
  /// Send, counting the backlog on success.
  ///
  /// The gauge increments before the send so a concurrent receiver can
  /// never observe the value while the counter still reads zero (which
  /// would wrap the depth to `usize::MAX` on decrement). A failed send
  /// rolls the increment back: no phantom backlog may stick.
  ///
  /// # Errors
  ///
  /// Returns the value back when the receiver is gone (shutdown).
  pub fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
    self.gauge.inc();
    match self.inner.send(value) {
      Ok(()) => Ok(()),
      Err(err) => {
        self.gauge.dec();
        Err(err)
      }
    }
  }

  /// The shared gauge (for census snapshots).
  #[must_use]
  pub fn gauge(&self) -> QueueGauge {
    self.gauge.clone()
  }

  /// Capacity-checked send: sheds (counted) past `cap` instead of growing
  /// without bound. Single-producer exact where only this sender feeds the
  /// channel (concurrent receives can only undercount, i.e. shed early —
  /// never overflow). Channels carrying clears must NOT use this: a shed
  /// clear ghosts. Reserved for best-effort fast paths (proc-events),
  /// where the periodic scan backstops every shed event.
  pub fn send_checked(&self, value: T, cap: usize) -> SendChecked {
    // Reserve-first: the reservation IS the backlog increment (the receiver
    // releases it), so concurrent senders serialize here instead of racing
    // a separate check-then-send.
    if !self.gauge.try_reserve(cap) {
      // A dead receiver with a full backlog must read `Closed` (callers
      // exit on it): `Shed` here would spin them forever, since no receive
      // will ever free space again.
      if !self.gauge.is_live() {
        return SendChecked::Closed;
      }
      self.gauge.shed();
      return SendChecked::Shed;
    }
    match self.inner.send(value) {
      Ok(()) => SendChecked::Sent,
      // Reserved but undeliverable: release the reservation, no phantom
      // backlog may stick (same discipline as `send` above).
      Err(_) => {
        self.gauge.dec();
        SendChecked::Closed
      }
    }
  }
}

/// `mpsc::Receiver` that releases its backlog count on every receive.
pub struct GaugeReceiver<T> {
  inner: mpsc::Receiver<T>,
  gauge: QueueGauge,
}

impl<T> std::fmt::Debug for GaugeReceiver<T> {
  /// Backlog depth only; queued values stay out of logs.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GaugeReceiver")
      .field("depth", &self.gauge.depth())
      .finish_non_exhaustive()
  }
}

impl<T> GaugeReceiver<T> {
  /// Blocking receive, releasing the backlog count on success.
  ///
  /// # Errors
  ///
  /// Returns [`mpsc::RecvError`] once every sender is gone (shutdown).
  pub fn recv(&self) -> Result<T, mpsc::RecvError> {
    self.inner.recv().inspect(|_| {
      self.gauge.dec();
    })
  }

  /// Bounded receive for interruptible loops, releasing the backlog count
  /// on success exactly like [`recv`](Self::recv).
  ///
  /// # Errors
  ///
  /// Returns [`mpsc::RecvTimeoutError::Timeout`] on deadline expiry (the
  /// caller re-checks its shutdown flag) and
  /// [`mpsc::RecvTimeoutError::Disconnected`] once every sender is gone.
  pub fn recv_timeout(
    &self,
    timeout: std::time::Duration,
  ) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
    self.inner.recv_timeout(timeout).inspect(|_| {
      self.gauge.dec();
    })
  }
}

impl<T> Drop for GaugeReceiver<T> {
  /// Mark the receiver gone so capacity-checked sends read `Closed`
  /// instead of shedding forever against a dead consumer.
  fn drop(&mut self) {
    self.gauge.live.store(false, Ordering::Relaxed);
  }
}

/// Page size in bytes, or `None` when `sysconf` refuses. Linux only;
/// elsewhere there is no `/proc` to read, so callers report absence.
#[cfg(target_os = "linux")]
#[must_use]
pub fn page_size_bytes() -> Option<u64> {
  // SAFETY: `sysconf` with `_SC_PAGESIZE` takes no pointer and has no
  // failure mode beyond returning -1.
  let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
  (page > 0).then_some(page as u64)
}

/// Resident set size in bytes, Linux only (`/proc/self/statm` resident
/// field × page size). `None` elsewhere or when the kernel won't tell.
#[cfg(target_os = "linux")]
#[must_use]
pub fn rss_bytes() -> Option<u64> {
  let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
  let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
  resident_pages.checked_mul(page_size_bytes()?)
}

/// Resident set high-water mark in bytes, Linux only (`VmHWM` from
/// `/proc/self/status`).
///
/// The current RSS says where the process is now; this says the peak it
/// ever reached. A memory change is not considered verified without both,
/// because a floor that stays put while the peak climbs is a growth the
/// current value cannot show. `None` elsewhere or when unreadable.
#[cfg(target_os = "linux")]
#[must_use]
pub fn rss_hwm_bytes() -> Option<u64> {
  for line in std::fs::read_to_string("/proc/self/status").ok()?.lines() {
    if let Some(rest) = line.strip_prefix("VmHWM:") {
      let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
      return kib.checked_mul(1024);
    }
  }
  None
}

/// Live vs free heap bytes from the allocator (glibc `mallinfo2`):
/// distinguishes live objects from retained arenas — `live` growing is a
/// real growth, `free` growing with a flat RSS floor is the allocator
/// holding pages.
///
/// `None` off glibc-Linux (musl, macOS, Windows), where the census simply
/// carries no split. Permanent telemetry, not DIAG: one line per census,
/// zero retained state.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[must_use]
pub fn heap_numbers() -> Option<(u64, u64)> {
  // SAFETY: `mallinfo2` only reads allocator statistics; it neither
  // allocates nor mutates anything.
  let info = unsafe { libc::mallinfo2() };
  #[allow(clippy::cast_possible_truncation)]
  Some((info.uordblks as u64, info.fordblks as u64))
}

/// No `mallinfo2` off glibc: report absence rather than guessing.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
#[must_use]
pub fn heap_numbers() -> Option<(u64, u64)> {
  None
}

/// Non-Linux targets have no cheap self-RSS source: report absence and let
/// the census line render `n/a` instead of guessing.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn rss_bytes() -> Option<u64> {
  None
}

/// No `/proc` off Linux: report absence so the census renders `n/a`
/// instead of guessing.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn rss_hwm_bytes() -> Option<u64> {
  None
}

/// One resource census: client counts, queue depths and memory.
///
/// Constructible: census owners (the daemon core) build one per sample.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
  /// Resident bytes (`None` off-Linux or when unreadable).
  pub rss_bytes: Option<u64>,
  /// Resident high-water mark, the peak the process ever reached. The
  /// current value alone cannot show a floor that holds while the peak
  /// climbs, which is exactly the shape a ratchet takes.
  pub rss_hwm_bytes: Option<u64>,
  /// Live vs free heap bytes from the allocator, read as one sample.
  /// `None` off glibc-Linux. `live` growing is a real growth; `free`
  /// growing with a flat RSS floor is the allocator holding pages — the
  /// pair is what separates the two.
  pub heap: Option<(u64, u64)>,
  /// JSON bridge consumers.
  pub bridge_json: usize,
  /// MessagePack bridge consumers.
  pub bridge_msgpack: usize,
  /// Game WebSocket consumers.
  pub ws: usize,
  /// Watch (proc-events subscription) queue depth.
  pub watch_depth: usize,
  /// Process-event queue depth.
  pub proc_depth: usize,
  /// Game-event queue depth.
  pub ws_depth: usize,
  /// Proc-events the bounded watch queue shed since boot. Diagnostic
  /// counterpart to `watch_depth`: depth says how deep the backlog is,
  /// this says how much was dropped — a non-zero value means the watcher
  /// fell behind, which depth alone can hide (the queue recovers to a
  /// steady depth while events are silently lost).
  pub shed_watch: usize,
}

/// Render the census line. Pure function so tests pin the shape.
#[must_use]
pub fn format_resource_stats(reason: &str, snapshot: &StatsSnapshot) -> String {
  format!(
    "[adsum] stats ({reason}): rss={}(hwm {}) heap=live {}/free {} bridge=json:{}+msgpack:{} ws={} queues=watch:{}+proc:{}+ws:{} shed:watch:{}",
    snapshot.rss_mb(),
    snapshot.rss_hwm_mb(),
    snapshot.heap_live_mb(),
    snapshot.heap_free_mb(),
    snapshot.bridge_json,
    snapshot.bridge_msgpack,
    snapshot.ws,
    snapshot.watch_depth,
    snapshot.proc_depth,
    snapshot.ws_depth,
    snapshot.shed_watch,
  )
}

impl StatsSnapshot {
  /// Resident bytes as `12.3MB`, or `n/a` when unreadable/off-Linux.
  fn rss_mb(&self) -> String {
    self
      .rss_bytes
      .map(|bytes| format!("{:.1}MB", bytes as f64 / 1_048_576.0))
      .unwrap_or_else(|| "n/a".to_string())
  }

  /// Resident high-water mark as `12.3MB`, or `n/a` when unreadable.
  fn rss_hwm_mb(&self) -> String {
    self
      .rss_hwm_bytes
      .map(|bytes| format!("{:.1}MB", bytes as f64 / 1_048_576.0))
      .unwrap_or_else(|| "n/a".to_string())
  }

  /// Live heap as `12.3MB`, or `n/a` where the allocator will not say.
  fn heap_live_mb(&self) -> String {
    self
      .heap
      .map(|(live, _)| format!("{:.1}MB", live as f64 / 1_048_576.0))
      .unwrap_or_else(|| "n/a".to_string())
  }

  /// Free heap still held by the allocator, or `n/a`.
  fn heap_free_mb(&self) -> String {
    self
      .heap
      .map(|(_, free)| format!("{:.1}MB", free as f64 / 1_048_576.0))
      .unwrap_or_else(|| "n/a".to_string())
  }
}
