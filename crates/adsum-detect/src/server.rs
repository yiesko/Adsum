//! Scanner orchestration: [`ProcessServer`] owns the generation pointer,
//! the scan/refresh/watcher threads and every channel.
//!
//! Reads scale lock-free: the bundle behind [`ArcSwap`], exclusions and
//! Steam roots behind `RwLock`. Only genuinely mutable per-tick state
//! (caches, pid sets, wake handles) sits behind short `Mutex` sections.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use adsum_steam::SteamLibraries;
use arc_swap::ArcSwap;
#[cfg(not(target_os = "linux"))]
use sysinfo::System;

use crate::bundle::{DetectablesBundle, initial_bundle};
use crate::cache::AppIdMemo;
use crate::cache::ProcCache;
use crate::db::{DetectableActivity, Exclusions};
use crate::heroic::HeroicApps;
use crate::platform::memory::release_parse_arenas;
use crate::refresh::RefreshConfig;
use crate::types::{ProcessDetectedEvent, ProcessEventListeners};

// 0.36.1 paths kept working after the module split: these items now
// live in `runtime`, re-exported here so existing imports keep building.
pub use super::runtime::{ScanGuard, idle_wait};

pub struct ProcessServer {
  /// Current detection generation (see [`DetectablesBundle`]): lock-free
  /// reads via [`ArcSwap`], whole-generation swaps by writers. Custom
  /// overrides live in the same bundle, so user appends can never tear
  /// against the main patterns either. The `Arc` wrapper makes every
  /// clone share the same generation pointer, so hourly refreshes reach
  /// the scan loop's clone too.
  pub(crate) detectables: Arc<ArcSwap<DetectablesBundle>>,
  /// Serializes bundle writers (hourly refresh vs override rebuilds):
  /// both read the current generation, build, then store, so without
  /// this a refresh landing inside an append's window silently discards
  /// the override (or the refreshed main list). Readers are unaffected:
  /// they keep loading the lock-free pointer.
  pub(crate) writer_lock: Arc<Mutex<()>>,
  pub(crate) scanning: Arc<AtomicBool>,
  /// Double-`start` guard: a second scan generation would orphan the
  /// first loop's wake handle and double-emit EXEC hits.
  pub(crate) started: Arc<AtomicBool>,
  /// Directed shutdown, set by `shutdown`: every thread spawned by
  /// `start` observes it and exits, so `join` returns promptly. Plain
  /// flag (no lock needed: set once, read in loops).
  pub(crate) shutdown: Arc<AtomicBool>,
  /// Set by the EXEC fast path whenever it publishes a hit, consumed by
  /// the scan loop: an EXEC-published game that dies before any poll
  /// observes it would otherwise never emit its clear (the delta would
  /// see two identical empty snapshots). Forcing one full emission per
  /// EXEC publication closes that hole; repeats dedup downstream.
  pub(crate) scan_dirty: Arc<AtomicBool>,
  /// Threads registered for shutdown unpark (refresh, watcher retry):
  /// each registers once at startup; `shutdown` unparks all so no
  /// `park_timeout` sleeps out its full duration. The scan loop keeps
  /// its own single-slot `scan_wake` (EXIT wakes share it), which
  /// `shutdown` unparks too.
  pub(crate) shutdown_wake: Arc<Mutex<Vec<std::thread::Thread>>>,
  /// Join handles of every thread `start` spawned, taken by `join`.
  /// Empty before `start` and after `join` (a second `join` is safe).
  pub(crate) worker_handles: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,

  pub event_sender: adsum_telemetry::GaugeSender<ProcessDetectedEvent>,

  pub(crate) event_listeners: Arc<Mutex<ProcessEventListeners>>,

  /// Hourly database-refresh inputs (source URL, toggle, startup seeds).
  /// Read once when the refresh thread spawns; immutable afterwards.
  pub(crate) refresh: RefreshConfig,
  /// Pids of the currently detected games (refreshed every scan tick,
  /// plus event-driven EXEC hits). Lets the proc-events watcher wake the
  /// scan loop the moment a TRACKED game exits — untracked exits never
  /// cause a scan.
  pub detected_pids: Arc<Mutex<FxHashSet<u64>>>,
  /// Memoized SteamAppId per pid: environ never changes after exec, so
  /// one read per process lifetime suffices (environ is kilobytes — the
  /// biggest per-process cost in the profiler). Invalidated by EXEC (the
  /// watcher drops the entry before reclassifying) and by death (swept
  /// every tick against the live pid set). Bounded by live process count.
  /// Each entry carries a sequence number: a `drop_appid` racing an
  /// in-flight read bumps it, and the stale read is discarded instead of
  /// pinning a pre-exec environ for the pid lifetime.
  pub(crate) appid_cache: Arc<Mutex<FxHashMap<u64, AppIdMemo>>>,
  /// Scan thread handle for early wakeups (proc-events EXIT of a tracked
  /// game). Registered by the scan thread itself on startup.
  pub(crate) scan_wake: Arc<Mutex<Option<std::thread::Thread>>>,
  /// Last scan-loop iteration start. EXIT wakes are debounced against it:
  /// Proton games spawn/die short-lived helpers constantly, and every one
  /// of them matches the game — without this, tracked exits unpark the
  /// loop several times per second (measured 0.76s effective cadence
  /// instead of 5s during NFS). Minimum 1s between early scans.
  pub last_scan: Arc<Mutex<std::time::Instant>>,
  /// Discord detection exclusions (installer/crash-reporter basenames +
  /// regexes): excluded processes are dropped before any matching.
  /// Empty until [`ProcessServer::set_exclusions`] (startup fetch) or the
  /// hourly refresh fills it; empty behaves exactly like no exclusions.
  /// Reads dominate (one per process per tick), hence `RwLock`.
  pub(crate) exclusions: Arc<RwLock<Exclusions>>,
  /// Steam install-dir -> AppId (VDF provider): refreshed once per scan
  /// tick when a `libraryfolders.vdf` changed, consulted on path misses.
  pub(crate) steam_libraries: Arc<RwLock<SteamLibraries>>,
  /// Heroic app name -> Epic namespace (Library provider): refreshed once
  /// per scan tick when the cached Legendary library changed, consulted
  /// when the sandbox id does not resolve.
  pub(crate) heroic_apps: Arc<RwLock<HeroicApps>>,
  /// Application IDs never published by the scan thread (coexistence with
  /// a richer publisher elsewhere). Filtered right after the scan, so an
  /// ignored-only result behaves exactly like no game: null event, clear.
  /// Hash set (built once): consulted per detected game per tick.
  pub(crate) ignored_ids: HashSet<String>,
  /// Event-driven proc-events watcher (netlink `cn_proc` fast path)
  /// on/off. Plain bool (no lock needed: written once via
  /// [`ProcessServer::set_proc_events`] before [`ProcessServer::start`],
  /// read once there). `true` unless `--no-proc-events` opted out.
  /// Read on every target: it gates `cache_mode_live`, which is what
  /// keeps the incremental mode (and thus the cache) off wherever no
  /// watcher exists to invalidate it.
  pub enable_proc_events: bool,
  /// Whether the watcher socket is currently believed up. Set around the
  /// blocking `watch()` call (Linux only); read every scan tick to gate
  /// idle backoff. Starts `false` (pure polling) so macOS, Windows,
  /// disabled watchers and failure windows never back off. `Relaxed` would
  /// suffice for a cadence hint, but `Acquire`/`Release` is free here and
  /// keeps the watcher-state reasoning uniform.
  pub(crate) watcher_live: Arc<AtomicBool>,
  /// The proc-events watcher's backlog gauge, set once when the watcher
  /// spawns. A cloned handle to the same counters the channel ends share,
  /// so the census reads shed totals without a second copy of the state
  /// and without the watcher having to mirror anything. `None` until
  /// `start` runs a Linux watcher.
  pub(crate) watch_gauge: Arc<Mutex<Option<adsum_telemetry::QueueGauge>>>,
  /// Per-pid classification cache of the incremental scan mode: one
  /// entry per live pid, swept against the enumerated set every tick,
  /// flushed on any watcher loss signal (see the `cache::procs` module
  /// docs for the full invalidation matrix). Consulted on every target
  /// by the shared tick orchestration; only Linux can currently reach
  /// the incremental branch (`cache_mode_live` needs the netlink
  /// watcher), so elsewhere it simply stays empty.
  pub(crate) proc_cache: Arc<Mutex<ProcCache>>,
  /// Invalidation flag set by the watcher on every loss channel it can
  /// observe (sequence gap, backlog shed, socket overrun, death and
  /// resubscribe): the next tick consumes it and flushes the cache, so
  /// a lost EXEC can never leave a stale classification trusted. Plain
  /// flag (set from one thread, consumed by the scan loop).
  pub(crate) cache_dirty: Arc<AtomicBool>,

  #[cfg(not(target_os = "linux"))]
  pub(crate) sysinfo: Arc<Mutex<System>>,
}

impl Clone for ProcessServer {
  /// Share every handle (generation pointer, channels, caches, locks).
  fn clone(&self) -> Self {
    Self {
      // Shared generation pointer: clones (scan loop, refresh thread)
      // must observe each other's swaps.
      detectables: Arc::clone(&self.detectables),
      writer_lock: Arc::clone(&self.writer_lock),
      scanning: Arc::clone(&self.scanning),
      started: Arc::clone(&self.started),
      shutdown: Arc::clone(&self.shutdown),
      scan_dirty: Arc::clone(&self.scan_dirty),
      shutdown_wake: Arc::clone(&self.shutdown_wake),
      worker_handles: Arc::clone(&self.worker_handles),
      event_sender: self.event_sender.clone(),
      event_listeners: Arc::clone(&self.event_listeners),
      refresh: self.refresh.clone(),
      detected_pids: Arc::clone(&self.detected_pids),
      appid_cache: Arc::clone(&self.appid_cache),
      scan_wake: Arc::clone(&self.scan_wake),
      last_scan: Arc::clone(&self.last_scan),
      exclusions: Arc::clone(&self.exclusions),
      steam_libraries: Arc::clone(&self.steam_libraries),
      heroic_apps: Arc::clone(&self.heroic_apps),
      ignored_ids: self.ignored_ids.clone(),
      enable_proc_events: self.enable_proc_events,
      watcher_live: Arc::clone(&self.watcher_live),
      watch_gauge: Arc::clone(&self.watch_gauge),
      proc_cache: Arc::clone(&self.proc_cache),
      cache_dirty: Arc::clone(&self.cache_dirty),
      #[cfg(not(target_os = "linux"))]
      sysinfo: Arc::clone(&self.sysinfo),
    }
  }
}

impl ProcessServer {
  /// The watcher's backlog gauge, once the watcher has spawned.
  ///
  /// A clone, not a borrow: [`adsum_telemetry::QueueGauge`] is refcounted,
  /// so this handle reads the very depth/shed counters the channel ends
  /// update — no second counter to mirror or drift. `None` when the
  /// watcher is off, unsupported or not Linux.
  #[must_use]
  pub fn watch_gauge(&self) -> Option<adsum_telemetry::QueueGauge> {
    self
      .watch_gauge
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  /// Folds `custom` overrides into the initial build: one automaton
  /// construction instead of build-then-rebuild. Startup (and one-shot
  /// diagnostics) use this so staged overrides never cost a second full
  /// build per boot.
  pub fn new_with_custom(
    detectable: Vec<Arc<DetectableActivity>>,
    custom: Vec<DetectableActivity>,
    event_sender: adsum_telemetry::GaugeSender<ProcessDetectedEvent>,
    event_listeners: ProcessEventListeners,
    refresh: RefreshConfig,
    ignored_ids: Vec<String>,
  ) -> Self {
    let bundle = initial_bundle(detectable, custom);

    let server = ProcessServer {
      scanning: Arc::new(AtomicBool::new(false)),
      started: Arc::new(AtomicBool::new(false)),
      shutdown: Arc::new(AtomicBool::new(false)),
      scan_dirty: Arc::new(AtomicBool::new(false)),
      shutdown_wake: Arc::new(Mutex::new(Vec::new())),
      worker_handles: Arc::new(Mutex::new(Vec::new())),
      detectables: Arc::new(ArcSwap::new(bundle)),
      writer_lock: Arc::new(Mutex::new(())),
      event_sender,

      // Event listeners
      event_listeners: Arc::new(Mutex::new(event_listeners)),

      // Detectable database auto-refresh
      refresh,
      ignored_ids: ignored_ids.into_iter().collect(),
      enable_proc_events: true,
      watcher_live: Arc::new(AtomicBool::new(false)),
      watch_gauge: Arc::new(Mutex::new(None)),
      proc_cache: Arc::new(Mutex::new(ProcCache::default())),
      cache_dirty: Arc::new(AtomicBool::new(false)),
      exclusions: Arc::new(RwLock::new(Exclusions::default())),
      steam_libraries: Arc::new(RwLock::new(SteamLibraries::discover())),
      heroic_apps: Arc::new(RwLock::new(HeroicApps::discover())),
      detected_pids: Arc::new(Mutex::new(FxHashSet::default())),
      appid_cache: Arc::new(Mutex::new(FxHashMap::default())),
      scan_wake: Arc::new(Mutex::new(None)),
      last_scan: Arc::new(Mutex::new(std::time::Instant::now())),

      // sysinfo System
      #[cfg(not(target_os = "linux"))]
      sysinfo: Arc::new(Mutex::new(System::new())),
    };

    // One-time parse arenas are now garbage: steady state is the lean
    // structures just built.
    release_parse_arenas();

    server
  }

  /// Opt out of the event-driven proc-events watcher (called once,
  /// before [`ProcessServer::start`]). Polling continues either way.
  pub fn set_proc_events(&mut self, enable: bool) {
    self.enable_proc_events = enable;
  }

  /// Read guard over the current exclusion set, poison-tolerant like
  /// every other hot-path lock. Callers hoist it: one guard per scan
  /// tick (or per EXEC event) instead of one per process — the set is
  /// rewritten hourly at most, so nothing observable can move under a
  /// held guard.
  pub fn exclusions_read(&self) -> std::sync::RwLockReadGuard<'_, Exclusions> {
    self.exclusions.read().unwrap_or_else(|e| e.into_inner())
  }
}

#[cfg(test)]
#[path = "../tests/unit/server/mod.rs"]
mod tests;
