//! Incremental scan-mode tests: cache reuse, and every invalidation
//! channel degrading to (at most) one full-cost re-read — never to a
//! stale classification surviving a tick.
//!
//! Linux-only like the mode itself: the tests drive a real `/bin/sleep`
//! child through the real `/proc` sweep, with `watcher_live` forced on
//! (the unit-test stand-in for a subscribed netlink socket).

#![cfg(target_os = "linux")]

use super::*;
use crate::db::DetectableActivity;
use crate::refresh::RefreshConfig;
use crate::scratch::ScanScratch;
use crate::server::ProcessServer;
use crate::types::{Exec, ProcessEventListeners};
use std::sync::atomic::Ordering;

/// A custom entry matching the `sleep` binary: the native automaton gets
/// the pattern `/sleep`, so the child's `/bin/sleep` argv[0] corroborates
/// on the component boundary (exactly the shape production matches).
fn sleep_db() -> Vec<DetectableActivity> {
  serde_json::from_value(serde_json::json!([
    {
      "id": "999001",
      "name": "Sleeper",
      "hook": true,
      "executables": [{"name": "sleep", "is_launcher": false, "os": "linux"}],
    }
  ]))
  .expect("fixture parses")
}

/// A second custom entry, for the generation-swap (epoch) test.
fn extra_db() -> Vec<DetectableActivity> {
  serde_json::from_value(serde_json::json!([
    {
      "id": "999002",
      "name": "Extra Game",
      "hook": true,
      "executables": [{"name": "extra-game-binary", "is_launcher": false, "os": "linux"}],
    }
  ]))
  .expect("fixture parses")
}

/// Server with the sleep entry pre-installed and the watcher "live".
fn live_server() -> ProcessServer {
  let (tx, _rx) = adsum_telemetry::QueueGauge::pair();
  let server = ProcessServer::new_with_custom(
    vec![],
    sleep_db(),
    tx,
    ProcessEventListeners::default(),
    RefreshConfig::default(),
    vec![],
  );
  // enable_proc_events defaults true; force the watcher-live belief that
  // gates incremental mode (no socket in unit tests — the mode's inputs
  // are the flag and the cache, both driven by hand here). The receiver
  // drops with this function: nothing in these tests publishes.
  server.watcher_live.store(true, Ordering::Release);
  drop(_rx);
  server
}

/// A real `/bin/sleep` child whose pid the sweep will classify.
///
/// Waits for the exec to land (cmdline showing `/bin/sleep`) before
/// returning: `spawn` returns once the fork happened, and a tick racing
/// the `execve` would read the parent's (or an empty) cmdline — in
/// production that window self-heals through the EXEC event's
/// reclassification, but the unit test needs the child deterministic.
fn spawn_sleeper() -> std::process::Child {
  let mut child = std::process::Command::new("/bin/sleep")
    .arg("60")
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("/bin/sleep spawns");
  let pid = child.id();
  for _ in 0..400 {
    if std::fs::read(format!("/proc/{pid}/cmdline"))
      .is_ok_and(|body| body.starts_with(b"/bin/sleep\0"))
    {
      return child;
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
  }
  // Never leave the child behind on the failure path (clippy's process
  // linter, and basic test hygiene).
  let _ = child.kill();
  let _ = child.wait();
  panic!("sleeper child never reached its exec");
}

/// One scan tick through the real entry point.
fn tick(
  server: &ProcessServer,
  processes: &mut Vec<Exec>,
  scratch: &mut ScanScratch,
) -> Vec<ScannedHit> {
  server
    .scan_for_processes(processes, scratch)
    .expect("scan succeeds")
}

/// The sentinel that proves reuse: a cached hit re-stamped with a start
/// no fresh classification could ever produce (epoch millis are ~1.7e12).
const SENTINEL_START: u64 = 12345;

/// Stamp the child's cached hit with the sentinel.
fn plant_sentinel(server: &ProcessServer, pid: u64) {
  let mut cache = server.proc_cache.lock().unwrap_or_else(|e| e.into_inner());
  let entry = cache
    .get_mut(&pid)
    .expect("the child must be cached after a live tick");
  let hit = entry
    .hit
    .as_mut()
    .expect("the child matches the sleep entry");
  hit.start = SENTINEL_START;
}

/// The child's hit start in a tick's table, if present.
fn start_for(hits: &[ScannedHit], pid: u64) -> Option<u64> {
  hits.iter().find(|h| h.pid == pid).map(|h| h.start)
}

/// The core promise: a live watcher + a current-epoch entry means the
/// second tick REUSES the classification — no cmdline re-read, no
/// automaton walk — and returns it verbatim (sentinel start intact).
#[test]
fn a_warm_tick_reuses_the_cached_classification() {
  let server = live_server();
  let mut child = spawn_sleeper();
  let pid = child.id() as u64;
  let mut processes = Vec::new();
  let mut scratch = ScanScratch::default();

  let first = tick(&server, &mut processes, &mut scratch);
  assert!(
    start_for(&first, pid).is_some(),
    "the sleeper child must be detected on the first (cold) tick"
  );
  // The cold tick classified through the work list and cached the hit.
  assert!(
    server
      .proc_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .contains_key(&pid),
    "a live tick must cache every enumerated pid it read"
  );

  plant_sentinel(&server, pid);
  let second = tick(&server, &mut processes, &mut scratch);
  assert_eq!(
    start_for(&second, pid),
    Some(SENTINEL_START),
    "a warm tick must return the CACHED hit verbatim (sentinel start), \
     not a fresh classification"
  );

  let _ = child.kill();
  let _ = child.wait();
}

/// A bundle swap moves the epoch: every entry becomes foreign and the
/// next tick re-reads + reclassifies (the sentinel cannot survive).
#[test]
fn a_generation_swap_invalidates_every_entry() {
  let server = live_server();
  let mut child = spawn_sleeper();
  let pid = child.id() as u64;
  let mut processes = Vec::new();
  let mut scratch = ScanScratch::default();

  let _ = tick(&server, &mut processes, &mut scratch);
  plant_sentinel(&server, pid);

  // A custom append rebuilds the bundle under the writer lock: new Arc,
  // new epoch — exactly the hourly-refresh shape at smaller scale.
  server.append_detectables(extra_db());

  let after_swap = tick(&server, &mut processes, &mut scratch);
  let start = start_for(&after_swap, pid).expect("the sleeper still matches");
  assert_ne!(
    start, SENTINEL_START,
    "a foreign epoch must force reclassification, not reuse the cache"
  );
  assert!(
    start > 1_000_000_000_000,
    "the fresh stamp must be a real epoch-millis value, got {start}"
  );

  let _ = child.kill();
  let _ = child.wait();
}

/// The watcher's loss signal (gap/shed/overrun/restart) flushes the
/// cache: the next tick degrades to one full-cost re-read.
#[test]
fn the_dirty_flag_flushes_the_cache() {
  let server = live_server();
  let mut child = spawn_sleeper();
  let pid = child.id() as u64;
  let mut processes = Vec::new();
  let mut scratch = ScanScratch::default();

  let _ = tick(&server, &mut processes, &mut scratch);
  plant_sentinel(&server, pid);

  server.cache_dirty.store(true, Ordering::Release);
  let after_flush = tick(&server, &mut processes, &mut scratch);
  assert_ne!(
    start_for(&after_flush, pid),
    Some(SENTINEL_START),
    "a dirty flag must flush: the sentinel cannot survive"
  );
  assert!(
    !server.take_cache_dirty(),
    "the tick must consume the flag (single flush per signal)"
  );

  let _ = child.kill();
  let _ = child.wait();
}

/// Dead pids leave the cache on the next tick's sweep — the enumerated
/// set is the liveness truth, so a cache entry can never outlive its
/// process by more than one tick.
#[test]
fn dead_pids_are_swept_out_of_the_cache() {
  let server = live_server();
  let mut child = spawn_sleeper();
  let pid = child.id() as u64;
  let mut processes = Vec::new();
  let mut scratch = ScanScratch::default();

  let first = tick(&server, &mut processes, &mut scratch);
  assert!(start_for(&first, pid).is_some());

  let _ = child.kill();
  let _ = child.wait();

  let after_death = tick(&server, &mut processes, &mut scratch);
  assert!(
    start_for(&after_death, pid).is_none(),
    "a dead pid must vanish from the tick's table"
  );
  assert!(
    !server
      .proc_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .contains_key(&pid),
    "the sweep must drop the dead entry, not merely hide it"
  );
}

/// Watcher dead (or disabled): the tick runs the classic full sweep and
/// never consults the cache — the sentinel is invisible, the detection
/// still correct. This is the fallback every loss window degrades to.
#[test]
fn a_dead_watcher_falls_back_to_the_full_sweep() {
  let server = live_server();
  let mut child = spawn_sleeper();
  let pid = child.id() as u64;
  let mut processes = Vec::new();
  let mut scratch = ScanScratch::default();

  let _ = tick(&server, &mut processes, &mut scratch);
  plant_sentinel(&server, pid);

  server.watcher_live.store(false, Ordering::Release);
  let full = tick(&server, &mut processes, &mut scratch);
  let start = start_for(&full, pid).expect("detection survives the fallback");
  assert_ne!(
    start, SENTINEL_START,
    "full mode must reclassify, never read the cache"
  );

  let _ = child.kill();
  let _ = child.wait();
}

/// A game that zombies between ticks (killed, not yet reaped) must not
/// hold its card: zombies are still enumerated — the `/proc` dir
/// outlives the address space — but their cmdline reads empty, and
/// "unreadable = absent" is the rule the full sweep has always applied.
/// The per-tick hit revalidation is what extends that rule to the cache.
#[test]
fn a_zombied_hit_is_revalidated_out() {
  let server = live_server();
  let mut child = spawn_sleeper();
  let pid = child.id() as u64;
  let mut processes = Vec::new();
  let mut scratch = ScanScratch::default();

  let first = tick(&server, &mut processes, &mut scratch);
  assert!(start_for(&first, pid).is_some());

  // Kill WITHOUT reaping: the child parks as a zombie until waited.
  let _ = child.kill();
  for _ in 0..400 {
    let zombied = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
      stat
        .rfind(')')
        .and_then(|close| stat[close + 1..].split_whitespace().next())
        .is_some_and(|state| state.starts_with('Z'))
    });
    if zombied {
      break;
    }
    std::thread::sleep(std::time::Duration::from_millis(5));
  }

  let second = tick(&server, &mut processes, &mut scratch);
  assert!(
    start_for(&second, pid).is_none(),
    "a zombie must not hold a detected card"
  );
  assert!(
    !server
      .proc_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .contains_key(&pid),
    "the failed revalidation must drop the entry, not merely hide it"
  );

  let _ = child.wait();
}

/// The dispatch-side seams in isolation: `cache_proc` records (misses
/// included — a verified miss is a classification too), `uncache_proc`
/// forgets, and the dirty flag round-trips through `take_cache_dirty`.
#[test]
fn cache_helpers_record_and_forget() {
  let server = live_server();
  let bundle = server.bundle();

  // A cached MISS (None) with the current epoch.
  server.cache_proc(4242, bundle.epoch, None, false);
  {
    let cache = server.proc_cache.lock().unwrap_or_else(|e| e.into_inner());
    let entry = cache.get(&4242).expect("inserted");
    assert_eq!(entry.epoch, bundle.epoch);
    assert!(entry.hit.is_none());
    assert!(!entry.obs);
  }
  server.uncache_proc(4242);
  assert!(
    !server
      .proc_cache
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .contains_key(&4242)
  );

  assert!(!server.take_cache_dirty(), "clean by construction");
  server.cache_dirty.store(true, Ordering::Release);
  assert!(server.take_cache_dirty(), "the first take consumes");
  assert!(!server.take_cache_dirty(), "the second sees clean");
}

/// The flush releases capacity, not just entries: a build storm's bucket
/// count must not be pinned for the session (same rule as both sweeps).
#[test]
fn flush_releases_capacity() {
  let server = live_server();
  let bundle = server.bundle();
  for pid in 0..4096u64 {
    server.cache_proc(pid, bundle.epoch, None, false);
  }
  let spiked = server
    .proc_cache
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .capacity();
  assert!(spiked >= 4096);
  server.flush_proc_cache();
  let cache = server.proc_cache.lock().unwrap_or_else(|e| e.into_inner());
  assert!(cache.is_empty());
  assert!(
    cache.capacity() * 64 <= spiked,
    "flush must shrink the table, capacity {} after spike {}",
    cache.capacity(),
    spiked
  );
}
